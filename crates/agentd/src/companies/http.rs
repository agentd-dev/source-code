// SPDX-License-Identifier: AGPL-3.0-only
//! Every request agentd sends a Company, over the one credentialed helper
//! (`auth::http`). Each one is held to the same rules, whatever it carries:
//!
//! - HTTPS, to a host the company entry's `hosts` admit (RFC 0047 §5.5.1,
//!   the host rule) — a URL a Company's documents name is dialled only there;
//! - through the SSRF guard: resolved once, every address classified, and
//!   loopback, private and link-local refused unless the entry sets
//!   `allow_private` (`Dial::Vetted`);
//! - no token in a URL (§4.3: "Personal Agents MUST NOT send tokens in a
//!   URL"), and no redirect followed on a request that carries a credential:
//!   a `3xx` there is a refusal, so nothing is re-sent to wherever it points.
//!
//! A discovery document is fetched without credentials and may redirect, to
//! HTTPS only (§3) and at most three times, each hop to an admitted host; its
//! body is capped and its JSON must not name a member twice
//! (Guides #both-transport).

use std::time::Duration;

use serde_json::Value;

use crate::auth::http::{self as ahttp, Dial, Reply};
use crate::config::settings::companies::{host_matches, host_of, is_https_url};

/// The most redirects a discovery fetch follows.
pub const MAX_REDIRECTS: usize = 3;
/// The largest discovery document read.
pub const MAX_DOCUMENT: usize = 100 * 1024;
/// How long a discovery fetch may take.
pub const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a Company's requests may go, and how they are dialled.
#[derive(Debug, Clone)]
pub struct Dialer {
    /// The company entry, for every refusal's message.
    pub company: String,
    /// Host globs a URL must match.
    pub hosts: Vec<String>,
    pub allow_private: bool,
    pub timeout: Duration,
}

impl Dialer {
    /// Whether `url` may be dialled: HTTPS, its host admitted.
    pub fn admits(&self, url: &str) -> Result<(), Refused> {
        if !is_https_url(url) {
            return Err(Refused::NotHttps(url.to_string()));
        }
        let host = host_of(url).unwrap_or_default();
        if self.hosts.iter().any(|p| host_matches(p, &host)) {
            Ok(())
        } else {
            Err(Refused::Host(host))
        }
    }

    fn dial(&self) -> Dial {
        Dial::Vetted {
            allow_private: self.allow_private,
        }
    }

    /// One credentialed request: `url` admitted, no token in it, and a
    /// redirect refused rather than followed.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Reply, String> {
        self.admits(url).map_err(|r| r.say(&self.company))?;
        if url
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv.starts_with("access_token=")))
        {
            return Err(format!(
                "company {}: a token in a URL is never sent",
                self.company
            ));
        }
        let reply = ahttp::send(method, url, headers, body, self.timeout, self.dial())?;
        if (300..400).contains(&reply.status) {
            return Err(format!(
                "company {}: {url} answered a redirect, which a request carrying a credential never follows",
                self.company
            ));
        }
        Ok(reply)
    }

    /// GET a document with no credential: HTTPS redirects followed, each hop
    /// admitted, the body capped and parsed as JSON naming no member twice.
    /// The URL it was served from after the redirects, its JSON and its
    /// cache lifetime.
    pub fn document(&self, url: &str) -> Result<Document, Refused> {
        let mut at = url.to_string();
        let mut hops = 0;
        loop {
            self.admits(&at)?;
            let reply = ahttp::get(
                &at,
                &[("Accept", "application/json")],
                DOCUMENT_TIMEOUT.min(self.timeout),
                self.dial(),
            )
            .map_err(Refused::Fetch)?;
            if (300..400).contains(&reply.status) {
                let to = reply.header("location").unwrap_or_default().to_string();
                let to = absolute(&at, &to);
                if !to.starts_with("https://") {
                    return Err(Refused::Redirect(to));
                }
                hops += 1;
                if hops > MAX_REDIRECTS {
                    return Err(Refused::Fetch(format!(
                        "{url}: more than {MAX_REDIRECTS} redirects"
                    )));
                }
                at = to;
                continue;
            }
            if reply.status != 200 {
                return Err(Refused::Fetch(format!("{at}: HTTP {}", reply.status)));
            }
            if reply.body.len() > MAX_DOCUMENT {
                return Err(Refused::Fetch(format!(
                    "{at}: the document is over {MAX_DOCUMENT} bytes"
                )));
            }
            let json =
                parse_unique(&reply.body).map_err(|e| Refused::Fetch(format!("{at}: {e}")))?;
            return Ok(Document {
                max_age: max_age(reply.header("cache-control")),
                json,
                url: at,
            });
        }
    }
}

/// A fetched document.
#[derive(Debug, Clone)]
pub struct Document {
    /// Where it was served from, after any redirects.
    pub url: String,
    pub json: Value,
    /// The `Cache-Control` `max-age`, when it gave one.
    pub max_age: Option<Duration>,
}

/// Why a URL was not dialled or a document not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NotHttps(String),
    /// A host the entry's `hosts` do not admit.
    Host(String),
    /// A redirect to anything but HTTPS (§3).
    Redirect(String),
    /// The fetch failed, or did not answer a document.
    Fetch(String),
}

impl Refused {
    pub fn say(&self, company: &str) -> String {
        match self {
            Refused::NotHttps(u) => format!("company {company}: {u} is not an HTTPS URL"),
            Refused::Host(h) => format!(
                "company {company}: {h} is not one of the entry's hosts (services.{company}.hosts)"
            ),
            Refused::Redirect(to) => {
                format!(
                    "company {company}: the discovery document redirects to {to}, which is not HTTPS"
                )
            }
            Refused::Fetch(e) => format!("company {company}: {e}"),
        }
    }
}

/// `location` read against the URL it was answered for.
fn absolute(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    match crate::config::settings::companies::origin(base) {
        Some(o) if location.starts_with('/') => format!("{o}{location}"),
        _ => location.to_string(),
    }
}

/// A `Cache-Control` header's `max-age`; `no-store` and `no-cache` are none.
pub fn max_age(header: Option<&str>) -> Option<Duration> {
    let h = header?;
    let mut age = None;
    for d in h.split(',').map(str::trim) {
        let lower = d.to_ascii_lowercase();
        if lower == "no-store" || lower == "no-cache" {
            return Some(Duration::ZERO);
        }
        if let Some(n) = lower.strip_prefix("max-age=") {
            age = n.trim_matches('"').parse().ok().map(Duration::from_secs);
        }
    }
    age
}

/// `bytes` as JSON, refusing an object that names a member twice — which
/// `serde_json` would take the last of without a word, and two readers of
/// one document could then disagree about.
pub fn parse_unique(bytes: &[u8]) -> Result<Value, String> {
    use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};

    struct Unique(Value);
    struct V;

    impl<'de> serde::Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Unique, D::Error> {
            d.deserialize_any(V).map(Unique)
        }
    }

    impl<'de> Visitor<'de> for V {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(v.into())
        }
        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(v.into())
        }
        fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
            Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
        }
        fn visit_str<E>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.to_string()))
        }
        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_none<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut out = Vec::new();
            while let Some(Unique(v)) = seq.next_element()? {
                out.push(v);
            }
            Ok(Value::Array(out))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut out = serde_json::Map::new();
            while let Some(k) = map.next_key::<String>()? {
                if out.contains_key(&k) {
                    return Err(serde::de::Error::custom(format!(
                        "the member {k:?} is named twice"
                    )));
                }
                let Unique(v) = map.next_value()?;
                out.insert(k, v);
            }
            Ok(Value::Object(out))
        }
    }

    let mut de = serde_json::Deserializer::from_slice(bytes);
    let v = serde::Deserialize::deserialize(&mut de)
        .map(|Unique(v)| v)
        .map_err(|e| format!("not JSON: {e}"))?;
    de.end().map_err(|e| format!("not JSON: {e}"))?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_named_twice_is_refused_at_any_depth() {
        assert!(parse_unique(br#"{"a": 1, "b": {"c": [1, {"d": 2}]}}"#).is_ok());
        let e = parse_unique(br#"{"a": 1, "a": 2}"#).unwrap_err();
        assert!(e.contains("\"a\" is named twice"), "{e}");
        assert!(parse_unique(br#"{"x": [{"k": 1, "k": 1}]}"#).is_err());
        assert!(parse_unique(br#"{"x": 1} trailing"#).is_err());
    }

    #[test]
    fn cache_control_gives_a_max_age() {
        assert_eq!(
            max_age(Some("public, max-age=300")),
            Some(Duration::from_secs(300))
        );
        assert_eq!(max_age(Some("no-store")), Some(Duration::ZERO));
        assert_eq!(max_age(Some("private")), None);
        assert_eq!(max_age(None), None);
    }

    fn dialer() -> Dialer {
        Dialer {
            company: "shop".into(),
            hosts: vec!["example.com".into(), "*.example.com".into()],
            allow_private: false,
            timeout: Duration::from_secs(1),
        }
    }

    /// The host rule and HTTPS are judged before any socket opens, and a
    /// token in a URL is never sent.
    #[test]
    fn a_url_off_the_hosts_or_carrying_a_token_is_refused_unsent() {
        let d = dialer();
        assert!(d.admits("https://mcp.example.com/mcp").is_ok());
        assert_eq!(
            d.admits("https://evil.example/token"),
            Err(Refused::Host("evil.example".into()))
        );
        assert!(matches!(
            d.admits("http://example.com/"),
            Err(Refused::NotHttps(_))
        ));
        let e = d
            .send("GET", "https://evil.example/x", &[], &[])
            .unwrap_err();
        assert!(e.contains("not one of the entry's hosts"), "{e}");
        let e = d
            .send("GET", "https://api.example.com/x?access_token=t", &[], &[])
            .unwrap_err();
        assert!(e.contains("a token in a URL"), "{e}");
    }
}
