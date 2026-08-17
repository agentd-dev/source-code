// SPDX-License-Identifier: Apache-2.0
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
//! change its mind between the two.
//!
//! Delivery is best-effort by design: a webhook that is down must not fail the
//! task it was reporting on. Failures are logged and dropped, never retried into
//! a queue that could outlive the task.

use serde_json::{Value, json};
use std::time::Duration;

use crate::a2a::tasks::PushTarget;

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
/// (`security.egress.allow_private`), not the caller's: on a cluster where the
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
/// The receiver gets the event exactly as the streaming caller would have seen
/// it, so a client can share one handler between the two ways of being told.
pub fn deliver(target: &PushTarget, event: &Value, allow_private: bool) -> Result<(), String> {
    // Guarded again here, not only at registration: the name resolved once when
    // the caller registered, and nothing stops it resolving elsewhere now.
    check_url(&target.url, allow_private)?;
    let u = crate::net::http::Url::parse(&target.url).map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(event).map_err(|e| e.to_string())?;

    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("agentd/{}", crate::VERSION)),
    ];
    if !target.token.is_empty() {
        // The caller's own token, echoed so the receiver can distinguish a real
        // delivery from anything else that finds the URL.
        headers.push(("x-a2a-notification-token".into(), target.token.clone()));
    }
    if let Some(b) = &target.bearer {
        headers.push(("authorization".into(), format!("Bearer {b}")));
    }
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let tcp = crate::net::http::connect_tcp(&u.host, u.port, DELIVERY_TIMEOUT)
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
/// returns it. The bearer agentd presents is deliberately absent: it is a
/// credential, and a read-back is not a reason to hand it out again.
pub fn to_wire(task_id: &str, t: &PushTarget) -> Value {
    let mut v = json!({"id": t.id, "taskId": task_id, "url": t.url});
    if !t.token.is_empty() {
        v["token"] = json!(t.token);
    }
    v
}

/// Read a caller's `TaskPushNotificationConfig` into a target.
///
/// The bearer is taken from `authentication.credentials` when the caller asked
/// for a bearer scheme — that is the one field of the authentication block
/// agentd can actually act on.
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
    let auth = v.get("authentication");
    let schemes = auth
        .and_then(|a| a.get("schemes"))
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter_map(Value::as_str)
                .any(|x| x.eq_ignore_ascii_case("bearer"))
        })
        .unwrap_or(false);
    let bearer = if schemes {
        auth.and_then(|a| a.get("credentials"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    Ok(PushTarget {
        id,
        url,
        token,
        bearer,
    })
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

    #[test]
    fn a_config_round_trips_without_leaking_the_credential() {
        let cfg = json!({
            "url": "https://hooks.example/agentd",
            "token": "caller-token",
            "authentication": {"schemes": ["Bearer"], "credentials": "secret-bearer"}
        });
        let t = from_wire(&cfg, "pc-1".into()).expect("a valid config");
        assert_eq!(t.url, "https://hooks.example/agentd");
        assert_eq!(t.token, "caller-token");
        assert_eq!(t.bearer.as_deref(), Some("secret-bearer"));

        // Reading it back returns the caller's own token but never the bearer
        // agentd would present.
        let wire = to_wire("task-1", &t);
        assert_eq!(wire["taskId"], "task-1");
        assert_eq!(wire["token"], "caller-token");
        assert!(wire.get("authentication").is_none(), "{wire}");
        assert!(!wire.to_string().contains("secret-bearer"), "{wire}");
    }

    #[test]
    fn a_config_without_a_url_is_not_a_config() {
        assert!(from_wire(&json!({"token": "t"}), "pc-1".into()).is_err());
        assert!(from_wire(&json!({"url": ""}), "pc-1".into()).is_err());
    }

    #[test]
    fn an_unasked_for_scheme_does_not_become_a_bearer() {
        // `credentials` without a bearer scheme is not a bearer — presenting it
        // as one would send a credential somewhere the caller did not ask.
        let cfg = json!({
            "url": "https://hooks.example/x",
            "authentication": {"schemes": ["ApiKey"], "credentials": "k"}
        });
        assert!(from_wire(&cfg, "p".into()).unwrap().bearer.is_none());
    }
}
