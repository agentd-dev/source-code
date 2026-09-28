// SPDX-License-Identifier: AGPL-3.0-only
//! **Push notifications**: telling a caller about a task instead of making it
//! watch one.
//!
//! A2A's streaming methods assume the caller can hold a connection open for as
//! long as the work takes. A caller that cannot — a serverless function, a queue
//! consumer, anything that would rather be woken — registers a webhook, and
//! agentd POSTs each update to it.
//!
//! ## Why this is the careful part
//!
//! The URL comes from the caller. That makes every delivery an outbound request
//! to an address a *peer* chose, which is the shape of an SSRF: point it at
//! `169.254.169.254` and agentd fetches cloud credentials on your behalf; point
//! it at an internal admin endpoint and agentd reaches somewhere the caller
//! cannot. So a target is guarded twice — refused at registration, when the
//! caller is present to be told why, and again at delivery, because DNS can
//! change its mind between the two. The delivery guard resolves once and dials
//! an address it vetted (`ssrf::connect_vetted`); handing the *name* to the
//! connect would re-resolve it and hand a hostile nameserver the second answer.
//!
//! Delivery is best-effort by design: a webhook that is down must not fail the
//! task it was reporting on. Failures are logged and dropped, never retried into
//! a queue that could outlive the task.

use serde_json::{Value, json};
use std::time::Duration;

use crate::a2a::tasks::{PushAuth, PushTarget};

/// How long one delivery may take. Short: a webhook is a notification, not a
/// conversation, and a slow receiver must not accumulate threads.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a caller-supplied webhook may be dialled at all.
///
/// Two independent rules. The **scheme** must be `https`, because a delivery
/// carries task content and the caller's own token; plaintext is allowed only to
/// loopback, for development. The **address** must pass the SSRF guard, and
/// loopback is not exempt from it — a peer that could make agentd POST to
/// `127.0.0.1` could reach agentd's own surfaces.
///
/// `allow_private` is the operator's decision
/// (`a2a.push.allow_private`), not the caller's: on a cluster where the
/// receiver legitimately lives on a private address, refusing every private
/// target would make the feature useless — but that has to be someone's
/// explicit choice.
pub fn check_url(url: &str, allow_private: bool) -> Result<(), String> {
    let u = crate::net::http::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
    if !u.is_tls() && !crate::net::http::is_loopback_host(&u.host) {
        return Err("a push endpoint must be https (or loopback for development)".into());
    }
    crate::net::ssrf::guard_host(&u.host, allow_private).map_err(|e| e.to_string())
}

/// POST one update to a registered target. Blocking; callers run it off the
/// reactor.
///
/// `event` is the body as built — a `StreamResponse` for a task delivery (see
/// [`crate::a2a::wire::push_body`]). The headers are the spec's: the A2A media
/// type, and `Authorization: <scheme> <credentials>` when the caller registered
/// authentication (§4.3.3). `X-A2A-Notification-Token` is a legacy courtesy:
/// 1.0 does not define it, but the official a2a-python 1.x receiver still reads
/// the caller's token from it, so a registered token keeps travelling there.
pub fn deliver(target: &PushTarget, event: &Value, allow_private: bool) -> Result<(), String> {
    // Guarded again here, not only at registration: the name resolved once when
    // the caller registered, and nothing stops it resolving elsewhere now. This
    // re-check carries the scheme rule; the address rule is what the dial below
    // enforces, on the addresses it actually connects to.
    check_url(&target.url, allow_private)?;
    let u = crate::net::http::Url::parse(&target.url).map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(event).map_err(|e| e.to_string())?;

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/a2a+json".into()),
        ("user-agent".into(), format!("agentd/{}", crate::VERSION)),
    ];
    if !target.token.is_empty() {
        // The caller's own token, echoed so the receiver can distinguish a real
        // delivery from anything else that finds the URL.
        headers.push(("x-a2a-notification-token".into(), target.token.clone()));
    }
    if let Some(a) = &target.auth {
        headers.push((
            "authorization".into(),
            format!("{} {}", a.scheme, a.credentials),
        ));
    }
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Dial an address the guard vetted, not the name: `connect_tcp` would
    // resolve a second time, and a peer who controls the authoritative DNS
    // would answer the check above with a public address and this connect with
    // `169.254.169.254`. TLS/SNI and the `Host` header below deliberately stay
    // on the hostname — connect by IP, verify by name.
    let tcp = crate::net::ssrf::connect_vetted(&u.host, u.port, DELIVERY_TIMEOUT, allow_private)
        .map_err(|e| e.to_string())?;
    let resp = if u.is_tls() {
        #[cfg(feature = "tls")]
        {
            let mut s = crate::net::tls::connect(tcp, &u.host, None).map_err(|e| e.to_string())?;
            crate::net::http::send(&mut s, &u.host_header(), "POST", &u.path, &refs, &body)
                .map_err(|e| e.to_string())?
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("an https push endpoint needs the 'tls' build feature".into());
        }
    } else {
        let mut s = tcp;
        crate::net::http::send(&mut s, &u.host_header(), "POST", &u.path, &refs, &body)
            .map_err(|e| e.to_string())?
    };
    if resp.is_success() {
        Ok(())
    } else {
        Err(format!("push endpoint answered {}", resp.status))
    }
}

/// The wire shape of a registered target, as `GetTaskPushNotificationConfig`
/// returns it. The credentials agentd presents are deliberately absent: they
/// are a secret, and a read-back is not a reason to hand one out again. The
/// scheme is echoed, so a caller can see what it registered was accepted.
pub fn to_wire(task_id: &str, t: &PushTarget) -> Value {
    let mut v = json!({"id": t.id, "taskId": task_id, "url": t.url});
    if !t.token.is_empty() {
        v["token"] = json!(t.token);
    }
    if let Some(a) = &t.auth {
        v["authentication"] = json!({"scheme": a.scheme});
    }
    v
}

/// Read a caller's `TaskPushNotificationConfig` into a target.
///
/// `authentication` is the spec's `AuthenticationInfo {scheme, credentials}`.
/// Whatever it holds ends up verbatim in a header line, so it is refused —
/// never quietly dropped, which would send the webhook out unauthenticated
/// while the caller believes otherwise — unless it can be sent as-is:
///
/// * `scheme` is REQUIRED by the proto and must be an RFC 9110 `token`
///   (`tchar`s only), so it cannot carry a space, a colon or a line break;
/// * `credentials` must be non-empty and free of control characters, so a CR
///   or LF cannot end the header and start another.
pub fn from_wire(v: &Value, id: String) -> Result<PushTarget, String> {
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("push config needs a url")?
        .to_string();
    let token = v
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let auth = match v.get("authentication") {
        None | Some(Value::Null) => None,
        Some(a) => Some(auth_of(a)?),
    };
    Ok(PushTarget {
        id,
        url,
        token,
        auth,
    })
}

/// One `AuthenticationInfo`, checked as [`from_wire`] describes.
fn auth_of(a: &Value) -> Result<PushAuth, String> {
    let field = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default();
    let (scheme, credentials) = (field("scheme"), field("credentials"));
    if scheme.is_empty() {
        return Err(if credentials.is_empty() {
            "authentication needs a scheme".into()
        } else {
            "authentication credentials need a scheme to be sent under".into()
        });
    }
    if !scheme.bytes().all(is_tchar) {
        return Err(format!(
            "authentication scheme {scheme:?} is not an HTTP token (RFC 9110 §5.6.2)"
        ));
    }
    if credentials.is_empty() {
        return Err("authentication needs credentials".into());
    }
    if credentials.chars().any(char::is_control) {
        return Err("authentication credentials may not contain control characters".into());
    }
    Ok(PushAuth {
        scheme: scheme.to_string(),
        credentials: credentials.to_string(),
    })
}

/// RFC 9110 §5.6.2 `tchar`: the characters an auth-scheme may be made of.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caller_supplied_url_is_refused_before_it_is_ever_dialled() {
        // The whole point: the address comes from a peer, so the obvious
        // attacks are the ones to close first.
        assert!(check_url("http://169.254.169.254/latest/meta-data/", false).is_err());
        assert!(check_url("https://10.0.0.5/internal", false).is_err());
        assert!(check_url("https://[::1]/x", false).is_err());
        // Plaintext to a public address is refused too — a notification carries
        // task content, and the caller's token rides with it.
        assert!(check_url("http://example.com/hook", false).is_err());
        // Loopback is not an exception to the address rule: a peer that could
        // make agentd POST to 127.0.0.1 could reach agentd's own surfaces.
        assert!(check_url("http://127.0.0.1:9000/hook", false).is_err());
        // …and an operator who has said private targets are legitimate gets
        // them, plaintext loopback included.
        assert!(check_url("https://10.0.0.5/internal", true).is_ok());
        assert!(check_url("http://127.0.0.1:9000/hook", true).is_ok());
        // The ordinary case: a public address over https. An IP literal, so
        // the test asserts the rule rather than the state of DNS.
        assert!(check_url("https://93.184.216.34/agentd", false).is_ok());
    }

    /// The spec's config, as a2a-rs hands it over: parsed into the typed
    /// `TaskPushNotificationConfig` and serialized back. Going through the
    /// type is the point — it is what strips any shape the spec does not have,
    /// so a test that fed raw JSON would pass against fields no caller can
    /// actually send.
    fn typed(v: Value) -> Value {
        let t: a2a_rs::domain::TaskPushNotificationConfig =
            serde_json::from_value(v).expect("a TaskPushNotificationConfig");
        serde_json::to_value(t).expect("serialize")
    }

    #[test]
    fn a_config_round_trips_without_leaking_the_credential() {
        let cfg = typed(json!({
            "taskId": "task-1",
            "url": "https://hooks.example/agentd",
            "token": "caller-token",
            "authentication": {"scheme": "Bearer", "credentials": "secret-bearer"}
        }));
        let t = from_wire(&cfg, "pc-1".into()).expect("a valid config");
        assert_eq!(t.url, "https://hooks.example/agentd");
        assert_eq!(t.token, "caller-token");
        let auth = t.auth.clone().expect("the 1.0 shape is read, not dropped");
        assert_eq!(
            (auth.scheme.as_str(), auth.credentials.as_str()),
            ("Bearer", "secret-bearer")
        );

        // Reading it back returns the caller's own token and the scheme it
        // registered, but never the credentials agentd would present.
        let wire = to_wire("task-1", &t);
        assert_eq!(wire["taskId"], "task-1");
        assert_eq!(wire["token"], "caller-token");
        assert_eq!(
            wire["authentication"],
            json!({"scheme": "Bearer"}),
            "{wire}"
        );
        assert!(!wire.to_string().contains("secret-bearer"), "{wire}");
        // …and the read-back is itself a config the spec's type accepts.
        typed(wire);
    }

    #[test]
    fn any_token_scheme_is_honoured_as_registered() {
        let cfg = typed(json!({
            "url": "https://hooks.example/x",
            "authentication": {"scheme": "Basic", "credentials": "dTpw"}
        }));
        let auth = from_wire(&cfg, "p".into()).unwrap().auth.unwrap();
        assert_eq!(auth.scheme, "Basic");
        assert_eq!(auth.credentials, "dTpw");
    }

    #[test]
    fn authentication_that_cannot_be_sent_as_is_is_refused() {
        let refused = |auth: Value| {
            let cfg = typed(json!({"url": "https://hooks.example/x", "authentication": auth}));
            from_wire(&cfg, "p".into()).expect_err(&format!("{cfg} must be refused"))
        };
        // The proto makes the scheme REQUIRED; credentials with nowhere to go
        // are not quietly sent as something else.
        assert!(refused(json!({"credentials": "k"})).contains("scheme"));
        assert!(refused(json!({})).contains("scheme"));
        // A scheme is an HTTP token: no space, no separator, no line break.
        refused(json!({"scheme": "Bearer x", "credentials": "k"}));
        refused(json!({"scheme": "Bea:rer", "credentials": "k"}));
        refused(json!({"scheme": "Bearer\r\nX-Evil: 1", "credentials": "k"}));
        // Credentials must exist and must not end the header early.
        refused(json!({"scheme": "Bearer"}));
        refused(json!({"scheme": "Bearer", "credentials": ""}));
        refused(json!({"scheme": "Bearer", "credentials": "k\r\nX-Evil: 1"}));
        refused(json!({"scheme": "Bearer", "credentials": "k\u{7f}"}));
        // No `authentication` at all is a config without one, not an error.
        let bare = typed(json!({"url": "https://hooks.example/x"}));
        assert!(from_wire(&bare, "p".into()).unwrap().auth.is_none());
    }

    #[test]
    fn a_config_without_a_url_is_not_a_config() {
        assert!(from_wire(&json!({"token": "t"}), "pc-1".into()).is_err());
        assert!(from_wire(&json!({"url": ""}), "pc-1".into()).is_err());
    }

    /// What a receiver actually sees: the A2A media type, the registered
    /// authentication as one `Authorization` line, and the legacy token header.
    #[test]
    fn a_delivery_carries_the_registered_authentication() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let seen = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut w = conn.try_clone().unwrap();
            let mut r = BufReader::new(conn);
            let mut head = Vec::new();
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                if l.trim().is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push(l.trim_end().to_string());
            }
            let mut body = vec![0u8; len];
            r.read_exact(&mut body).unwrap();
            w.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            (head, body)
        });
        let target = PushTarget {
            id: "p".into(),
            url,
            token: "caller-token".into(),
            auth: Some(PushAuth {
                scheme: "Basic".into(),
                credentials: "dTpw".into(),
            }),
        };
        // Loopback, so the operator's `allow_private` is what lets it through.
        deliver(&target, &json!({"task": {"id": "t"}}), true).expect("delivered");
        let (head, body) = seen.join().unwrap();
        let has = |h: &str| head.iter().any(|l| l.eq_ignore_ascii_case(h));
        assert!(has("authorization: Basic dTpw"), "{head:?}");
        assert!(has("content-type: application/a2a+json"), "{head:?}");
        assert!(has("x-a2a-notification-token: caller-token"), "{head:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"task": {"id": "t"}})
        );
    }
}
