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
//! - nothing before the Company's not-before instant ([`Pace`]): a `429` or
//!   `503` with `Retry-After` from any of its endpoints holds every request
//!   to it, from every task, until the wait is over (§4.3: "The Personal
//!   Agent MUST wait at least that long before sending the request again").
//!
//! A discovery document is fetched without credentials and may redirect, to
//! HTTPS only (§3) and at most three times, each hop to an admitted host; its
//! body is capped and its JSON must not name a member twice
//! (Guides #both-transport).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::auth::http::{self as ahttp, Dial, Failure, Reply};
use crate::config::settings::companies::{host_matches, host_of, is_https_url};
use crate::obs::log::Logger;

/// The most redirects a discovery fetch follows.
pub const MAX_REDIRECTS: usize = 3;
/// The largest discovery document read.
pub const MAX_DOCUMENT: usize = 100 * 1024;
/// How long a discovery fetch may take.
pub const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times a request that is safe to repeat is sent before its
/// failure is the caller's (RFC 0047 §5.9): token requests, the MCP
/// handshake and tool lists. A `tools/call` is never one of them.
pub const ATTEMPTS: u32 = 3;

/// The wait before attempt `attempt + 1`: 250 ms doubling, at most 2 s, each
/// between half and all of that (Guides #both-retries: "back off
/// exponentially with jitter, cap the delay, and limit the number of
/// attempts").
pub fn backoff(attempt: u32) -> Duration {
    let cap = Duration::from_millis(250 * (1u64 << attempt.saturating_sub(1).min(3)))
        .min(Duration::from_secs(2));
    let mut b = [0u8; 2];
    let frac = match crate::sec::random::fill(&mut b) {
        Ok(()) => f64::from(u16::from_le_bytes(b)) / f64::from(u16::MAX),
        Err(_) => 1.0,
    };
    cap.mul_f64(0.5 + frac / 2.0)
}

/// How far a Company's `Date` may stray from agentd's clock before it is
/// said (RFC 0047 §5.5.5): proofs and assertions live about a minute.
pub const CLOCK_SKEW: Duration = Duration::from_secs(30);

/// When a Company may next be sent anything, shared by every task and every
/// interface of one company entry (Guides #both-rate-limits: "keep track of
/// limits for each company and share them across your tasks"). A `429` or
/// `503` with `Retry-After` from any endpoint moves the instant later, cut to
/// [`crate::net::http::RETRY_AFTER_MAX`]; a request before it is refused
/// unsent. The same place notes, once, a `Date` that strays more than
/// [`CLOCK_SKEW`] from agentd's clock.
#[derive(Default)]
pub struct Pace {
    company: String,
    log: Option<Logger>,
    until: Mutex<Option<(Instant, SystemTime)>>,
    skew_said: AtomicBool,
}

impl std::fmt::Debug for Pace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pace")
            .field("company", &self.company)
            .field("until", &self.until())
            .finish()
    }
}

impl Pace {
    pub fn new(company: &str, log: Option<Logger>) -> Pace {
        Pace {
            company: company.to_string(),
            log,
            ..Pace::default()
        }
    }

    /// The instant before which nothing is sent, while it is ahead.
    pub fn until(&self) -> Option<SystemTime> {
        let u = self.until.lock().unwrap_or_else(|e| e.into_inner());
        u.filter(|(at, _)| Instant::now() < *at)
            .map(|(_, wall)| wall)
    }

    /// `Ok` when a request may be sent now; otherwise why not, and until
    /// when.
    pub fn check(&self) -> Result<(), String> {
        match self.until() {
            None => Ok(()),
            Some(wall) => Err(format!(
                "rate limited by {} until {}; nothing is sent to it before then",
                self.company,
                crate::obs::log::rfc3339_millis(wall)
            )),
        }
    }

    /// What a response says about pacing: a `429` or `503` with a
    /// `Retry-After` holds the Company until the wait is over.
    pub fn learn(&self, status: u16, retry_after: Option<Duration>) {
        let Some(wait) = retry_after.filter(|_| matches!(status, 429 | 503)) else {
            return;
        };
        let wait = wait.min(crate::net::http::RETRY_AFTER_MAX);
        let at = (Instant::now() + wait, SystemTime::now() + wait);
        let moved = {
            let mut u = self.until.lock().unwrap_or_else(|e| e.into_inner());
            match *u {
                Some((was, _)) if was >= at.0 => false,
                _ => {
                    *u = Some(at);
                    true
                }
            }
        };
        if !moved {
            return;
        }
        let secs =
            at.1.duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
        crate::obs::metrics::set_company_rate_limited_until(&self.company, secs);
        if let Some(log) = &self.log {
            log.info(
                "company.rate_limited",
                json!({"company": self.company, "until": crate::obs::log::rfc3339_millis(at.1),
                       "status": status}),
            );
        }
    }

    /// A response's `Date`, against agentd's clock: said once when they are
    /// more than [`CLOCK_SKEW`] apart.
    pub fn date(&self, header: Option<&str>) {
        let Some(now) = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64)
        else {
            return;
        };
        let Some(at) = header.and_then(|h| crate::net::http::http_date(h, now)) else {
            return;
        };
        let skew = at - now;
        if skew.unsigned_abs() <= CLOCK_SKEW.as_secs()
            || self.skew_said.swap(true, Ordering::SeqCst)
        {
            return;
        }
        if let Some(log) = &self.log {
            log.warn(
                "company.clock_skew",
                json!({"company": self.company, "skew_s": skew}),
            );
        }
    }

    /// What one reply says about pacing and the Company's clock.
    fn note(&self, reply: &Reply) {
        self.learn(reply.status, reply.retry_after());
        self.date(reply.header("date"));
    }
}

/// Where a Company's requests may go, and how they are dialled.
#[derive(Debug, Clone)]
pub struct Dialer {
    /// The company entry, for every refusal's message.
    pub company: String,
    /// Host globs a URL must match.
    pub hosts: Vec<String>,
    pub allow_private: bool,
    pub timeout: Duration,
    /// The company entry's not-before instant.
    pub pace: Arc<Pace>,
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

    /// One credentialed request: `url` admitted, no token in it, not before
    /// the Company's not-before instant, and a redirect refused rather than
    /// followed.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Reply, String> {
        self.send_reaching(method, url, headers, body)
            .map_err(|f| f.to_string())
    }

    /// [`Dialer::send`], saying how far a failed request got: refused before
    /// it was dialled, or by the connection, is [`Failure::Unsent`]; a
    /// redirect is an answer, so it is [`Failure::Unanswered`] — the request
    /// it answered was sent.
    pub fn send_reaching(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Reply, Failure> {
        self.may_send(url).map_err(Failure::Unsent)?;
        let reply = ahttp::send_reaching(method, url, headers, body, self.timeout, self.dial())?;
        self.pace.note(&reply);
        self.no_redirect(url, reply.status)?;
        Ok(reply)
    }

    /// One credentialed request whose answer is read as it arrives — a
    /// conversation's events, streamed (§7.6) — held to the rules of
    /// [`Dialer::send`], each read of it waiting at most `read_for`.
    pub fn stream(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        read_for: Duration,
    ) -> Result<crate::net::http::StreamingResponse<Box<dyn crate::net::http::Stream>>, Failure>
    {
        self.may_send(url).map_err(Failure::Unsent)?;
        let r = ahttp::send_streaming(method, url, headers, &[], read_for, self.dial())?;
        self.pace.learn(r.status, r.retry_after());
        self.pace.date(r.header("date"));
        self.no_redirect(url, r.status)?;
        Ok(r)
    }

    /// Whether a credentialed request to `url` may go: admitted, carrying no
    /// token, and not before the not-before instant.
    fn may_send(&self, url: &str) -> Result<(), String> {
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
        self.pace.check()
    }

    fn no_redirect(&self, url: &str, status: u16) -> Result<(), Failure> {
        if (300..400).contains(&status) {
            return Err(Failure::Unanswered(format!(
                "company {}: {url} answered a redirect, which a request carrying a credential never follows",
                self.company
            )));
        }
        Ok(())
    }

    /// GET a document with no credential: HTTPS redirects followed, each hop
    /// admitted, the body capped and parsed as JSON naming no member twice.
    /// The URL it was served from after the redirects, its JSON and its
    /// cache lifetime.
    pub fn document(&self, url: &str) -> Result<Document, Refused> {
        self.document_up_to(url, MAX_DOCUMENT)
    }

    /// [`Dialer::document`], the body capped at `cap` bytes.
    pub fn document_up_to(&self, url: &str, cap: usize) -> Result<Document, Refused> {
        let mut at = url.to_string();
        let mut hops = 0;
        loop {
            self.admits(&at)?;
            self.pace.check().map_err(Refused::Fetch)?;
            let reply = ahttp::get(
                &at,
                &[("Accept", "application/json")],
                DOCUMENT_TIMEOUT.min(self.timeout),
                self.dial(),
            )
            .map_err(Refused::Fetch)?;
            self.pace.note(&reply);
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
            if reply.body.len() > cap {
                return Err(Refused::Fetch(format!(
                    "{at}: the document is over {cap} bytes"
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
    /// Not an HTTPS URL with a plain host.
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
            Refused::NotHttps(u) => {
                format!("company {company}: {u} is not an HTTPS URL with a plain host")
            }
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
            pace: Arc::new(Pace::new("shop", None)),
        }
    }

    #[test]
    fn a_retry_backs_off_with_jitter_under_a_cap() {
        for attempt in 1..=6 {
            let cap = Duration::from_millis(250 * (1 << (attempt - 1).min(3)))
                .min(Duration::from_secs(2));
            let b = backoff(attempt);
            assert!(b >= cap / 2 && b <= cap, "attempt {attempt}: {b:?}");
        }
    }

    /// A `429` or `503` with `Retry-After` holds every request until the
    /// wait is over, refused unsent; a later, shorter wait does not shorten
    /// it, and a status that asks for no wait sets none.
    #[test]
    fn a_wait_the_company_asks_for_holds_every_request() {
        let pace = Pace::new("shop", None);
        pace.learn(500, Some(Duration::from_secs(60)));
        pace.learn(429, None);
        assert!(pace.check().is_ok(), "no wait was asked for");
        pace.learn(429, Some(Duration::from_secs(2)));
        let e = pace.check().unwrap_err();
        assert!(e.starts_with("rate limited by shop until "), "{e}");
        let first = pace.until().unwrap();
        pace.learn(503, Some(Duration::from_millis(10)));
        assert_eq!(pace.until(), Some(first), "a shorter wait keeps the longer");
        let d = Dialer {
            pace: Arc::new(pace),
            ..dialer()
        };
        let e = d
            .send("POST", "https://auth.example.com/token", &[], &[])
            .unwrap_err();
        assert!(e.starts_with("rate limited by shop"), "refused unsent: {e}");
        let e = d.document("https://example.com/x").unwrap_err();
        assert!(
            matches!(&e, Refused::Fetch(m) if m.starts_with("rate limited")),
            "{e:?}"
        );
        // A Company whose clock is far from agentd's is said once; a
        // `Date` that does not parse says nothing.
        let clock = Pace::new("shop", None);
        clock.date(Some("not a date"));
        clock.date(None);
        assert!(!clock.skew_said.load(Ordering::SeqCst));
        clock.date(Some("Sun, 06 Nov 1994 08:49:37 GMT"));
        assert!(clock.skew_said.load(Ordering::SeqCst));
        // Past the instant, a request may go again.
        let short = Pace::new("shop", None);
        short.learn(429, Some(Duration::from_millis(20)));
        assert!(short.check().is_err());
        std::thread::sleep(Duration::from_millis(40));
        assert!(short.check().is_ok());
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
        // A browser goes to evil.example.
        assert!(matches!(
            d.admits("https://evil.example\\@api.example.com/x"),
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
