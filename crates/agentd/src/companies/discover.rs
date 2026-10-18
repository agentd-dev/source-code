// SPDX-License-Identifier: AGPL-3.0-only
//! Discovery and verification of a declared Company (RFC 0047 §5.5.1, draft
//! §3–§3.2), and the supervisor's cache of what was verified.
//!
//! 1. `GET https://<endpoint host>/.well-known/poppy.json`, with no
//!    credential, following HTTPS redirects only (at most three), capped at
//!    100 KiB, a member named twice refused.
//! 2. A `protocol_version` whose major is not `0` is refused; a newer minor
//!    is used, its unknown fields ignored.
//! 3. `organization.domain` must be the host that served the document, after
//!    the redirects, ignoring ASCII case.
//! 4. The issuer's RFC 8414 metadata, at the well-known URL with the suffix
//!    inserted before the issuer's path: its `issuer` must equal
//!    `auth.issuer` byte for byte, and `poppy_domains` must list the domain,
//!    ignoring case.
//! 5. `ES256` must be among `dpop_signing_alg_values_supported`; a Company
//!    that publishes the algorithms only under the prefixed member issue #12
//!    reports is refused naming it. `token_endpoint` and
//!    `revocation_endpoint` are required, `authorization_endpoint` with
//!    `auth.direct` and `device_authorization_endpoint` with `auth.device`.
//! 6. **The host rule** (agentd's): every URL the two documents name must
//!    match the entry's `hosts` — or, when it names none, the endpoint's
//!    host, the domain, the subdomains of both, and the issuer's host.
//! 7. Every dial goes through the SSRF guard (`companies::http`).
//! 8. The documents are kept for their `Cache-Control` max-age, clamped to a
//!    minute and a day (five minutes when they give none), and fetched again
//!    at the first use after that. A refetch that fails keeps the last
//!    verified documents for a day after they were verified; one that names
//!    another issuer suspends the Company until a restart; one that changes
//!    the MCP APIs changes no server until a restart.
//!
//! Each Company's MCP APIs are named here too: after the entry when it lists
//! one, by `mcp_names` when it lists several.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::http::{Dialer, Pace, Refused};
use super::wire;
use crate::config::settings::companies::{Interface, SignIn, host_of, is_https_url};

/// The shortest and longest a verified document is kept.
pub const MIN_MAX_AGE: Duration = Duration::from_secs(60);
pub const MAX_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How long a document that gives no lifetime is kept.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(300);
/// How long the last verified documents serve while a refetch keeps failing.
pub const STALE_FOR: Duration = Duration::from_secs(24 * 3600);

/// What discovery needs of a company entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// `https://<host>[:port]`.
    pub endpoint: String,
    pub hosts: Option<Vec<String>>,
    pub allow_private: bool,
    pub timeout: Duration,
    /// The entry's MCP servers, by name, with the `url` `mcp_names` gives.
    pub servers: Vec<(String, Option<String>)>,
    /// The Company's not-before instant, shared by every dial to it.
    pub pace: Arc<Pace>,
    /// The sign-in types agentd may start there, in order.
    pub sign_in: Vec<SignIn>,
    /// The most scopes agentd asks the User for there.
    pub scopes: Vec<String>,
    /// The interfaces agentd uses there.
    pub interfaces: Vec<Interface>,
    /// The OpenAPI operations agentd may call there, `METHOD /path-template`.
    pub api_allow: Vec<String>,
    /// The servers the entry's operation tools are named under, by name, with
    /// the OpenAPI API `url` `api_names` gives.
    pub apis: Vec<(String, Option<String>)>,
}

impl Entry {
    pub fn from_settings(
        name: &str,
        svc: &crate::config::settings::Service,
        timeout: Duration,
    ) -> Entry {
        Entry {
            name: name.to_string(),
            endpoint: svc.endpoint.trim_end_matches('/').to_string(),
            hosts: svc.hosts.clone(),
            allow_private: svc.allow_private.unwrap_or(false),
            timeout: svc.timeout.map(|d| d.0).unwrap_or(timeout),
            servers: crate::config::settings::companies::server_names(name, svc),
            pace: Arc::new(Pace::new(name, None)),
            sign_in: crate::config::settings::companies::sign_in_of(svc),
            scopes: svc.scopes.clone().unwrap_or_default(),
            interfaces: crate::config::settings::companies::interfaces_of(svc),
            api_allow: svc.api_allow.clone().unwrap_or_default(),
            apis: crate::config::settings::companies::api_server_names(name, svc),
        }
    }
}

/// A listed endpoint, and the `resource` its tokens must be issued for when
/// the Company names one (draft §3.1, §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub url: String,
    pub resource: Option<String>,
}

/// A Company's documents, verified.
#[derive(Debug, Clone)]
pub struct Verified {
    pub company: String,
    /// `organization.name`, the Company's own choice of words.
    pub name: String,
    /// `organization.domain`, the verified host.
    pub domain: String,
    pub issuer: String,
    pub token_endpoint: String,
    pub revocation_endpoint: String,
    pub authorization_endpoint: Option<String>,
    pub device_authorization_endpoint: Option<String>,
    /// The scopes Direct and Device Sign-In can grant (`auth.direct.scopes`,
    /// `auth.device.scopes`), when the Company offers them.
    pub direct_scopes: Option<Vec<String>>,
    pub device_scopes: Option<Vec<String>>,
    /// What the Company says each of its own scopes is (`auth.custom_scopes`),
    /// its own words.
    pub custom_scopes: BTreeMap<String, String>,
    /// The host globs every Company URL must match.
    pub hosts: Vec<String>,
    /// Each of the entry's MCP servers, by name, and its `url`.
    pub mcp: BTreeMap<String, String>,
    /// Every MCP API the discovery document lists, in its order.
    pub mcp_urls: Vec<String>,
    /// The Company Agent's `poppy` conversation endpoint, the first the
    /// document lists — other types are skipped (§7.1).
    pub conversation: Option<Listed>,
    /// Every OpenAPI API the document lists: the description's URL.
    pub openapi: Vec<Listed>,
    /// The Operations extension's endpoint, when the Company lists the
    /// version agentd speaks (Operations §2).
    pub operations: Option<Listed>,
    /// The interfaces agentd uses there.
    pub interfaces: Vec<Interface>,
    pub max_age: Duration,
    pub verified_at: Instant,
}

impl Verified {
    /// The dialer for every request to this Company.
    pub fn dialer(&self, entry: &Entry) -> Dialer {
        Dialer {
            company: self.company.clone(),
            hosts: self.hosts.clone(),
            allow_private: entry.allow_private,
            timeout: entry.timeout,
            pace: Arc::clone(&entry.pace),
        }
    }

    /// What `company.verified` says.
    pub fn line(&self) -> Value {
        json!({
            "company": self.company,
            "domain": self.domain,
            "issuer": self.issuer,
            "interfaces": self.interfaces.iter().map(|i| i.as_str()).collect::<Vec<_>>(),
            "servers": self.mcp.keys().collect::<Vec<_>>(),
            "max_age": self.max_age.as_secs(),
        })
    }
}

/// Why a Company's documents were not used: a short reason, the line that
/// says it, and for the host rule the field and the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub detail: String,
    pub field: Option<String>,
    pub host: Option<String>,
}

impl Refusal {
    fn new(reason: &'static str, detail: impl Into<String>) -> Refusal {
        Refusal {
            reason,
            detail: detail.into(),
            field: None,
            host: None,
        }
    }

    fn from_http(company: &str, r: Refused) -> Refusal {
        match &r {
            Refused::Redirect(_) => Refusal::new("redirect", r.say(company)),
            Refused::Host(h) => Refusal {
                field: Some("endpoint".into()),
                host: Some(h.clone()),
                ..Refusal::new("endpoint", r.say(company))
            },
            Refused::NotHttps(_) => Refusal::new("https", r.say(company)),
            Refused::Fetch(_) => Refusal::new("fetch", r.say(company)),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// How long verified documents are kept: their max-age clamped to
/// [`MIN_MAX_AGE`], [`MAX_MAX_AGE`], or [`DEFAULT_MAX_AGE`] without one. In a
/// debug or `internal-mocks` build `AGENTD_TEST_COMPANY_MAX_AGE_MS` replaces
/// it, so a test can see a refetch without waiting a minute.
pub fn lifetime(given: Option<Duration>) -> Duration {
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if let Some(ms) = std::env::var("AGENTD_TEST_COMPANY_MAX_AGE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    given
        .map(|d| d.clamp(MIN_MAX_AGE, MAX_MAX_AGE))
        .unwrap_or(DEFAULT_MAX_AGE)
}

fn string<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

/// Steps 1–7, and the MCP servers' names: the documents of `entry`,
/// verified.
pub fn discover(entry: &Entry) -> Result<Verified, Refusal> {
    let (verified, listed) = verify(entry)?;
    let mcp = servers_of(entry, &listed)?;
    Ok(Verified { mcp, ..verified })
}

/// The entry's MCP servers ([`name_servers`]), none when it does not use
/// the Company's MCP APIs.
fn servers_of(entry: &Entry, listed: &[String]) -> Result<BTreeMap<String, String>, Refusal> {
    if entry.interfaces.contains(&Interface::Mcp) {
        name_servers(entry, listed)
    } else {
        Ok(BTreeMap::new())
    }
}

/// What steps 1–7 come to: the verified documents and the MCP APIs they
/// list, or why they were not used.
type Verification = Result<(Verified, Vec<String>), Refusal>;

/// Steps 1–7: the verified documents, and the MCP APIs they list (not yet
/// named).
fn verify(entry: &Entry) -> Verification {
    let company = entry.name.as_str();
    let home = host_of(&entry.endpoint).unwrap_or_default();
    // Redirects may move within the entry's hosts, or by default to the
    // endpoint's host and its subdomains — never to a domain nobody named.
    let first_hosts = entry
        .hosts
        .clone()
        .unwrap_or_else(|| vec![home.clone(), format!("*.{home}")]);
    let fetch = Dialer {
        company: company.to_string(),
        hosts: first_hosts,
        allow_private: entry.allow_private,
        timeout: entry.timeout,
        pace: Arc::clone(&entry.pace),
    };
    let doc = fetch
        .document(&format!("{}{}", entry.endpoint, wire::DISCOVERY_PATH))
        .map_err(|r| Refusal::from_http(company, r))?;
    let poppy = &doc.json;
    let version = string(poppy, "/protocol_version").unwrap_or_default();
    if version.split('.').next() != Some(wire::PROTOCOL_MAJOR) {
        return Err(Refusal::new(
            "protocol_version",
            format!(
                "company {company}: protocol_version {version:?} — agentd uses documents of major version {}",
                wire::PROTOCOL_MAJOR
            ),
        ));
    }
    let served_by = host_of(&doc.url).unwrap_or_default();
    let domain = string(poppy, "/organization/domain")
        .unwrap_or_default()
        .to_string();
    if !domain.eq_ignore_ascii_case(&served_by) {
        return Err(Refusal::new(
            "domain",
            format!(
                "company {company}: organization.domain {domain:?} is not the host that served the document ({served_by})"
            ),
        ));
    }
    let Some(issuer) = string(poppy, "/auth/issuer").filter(|i| is_https_url(i)) else {
        return Err(Refusal::new(
            "auth",
            format!("company {company}: the discovery document names no HTTPS auth.issuer"),
        ));
    };
    let issuer = issuer.to_string();
    let issuer_host = host_of(&issuer).unwrap_or_default();
    let hosts = entry.hosts.clone().unwrap_or_else(|| {
        let mut h = vec![home.clone(), format!("*.{home}")];
        let d = domain.to_ascii_lowercase();
        for x in [d.clone(), format!("*.{d}"), issuer_host.clone()] {
            if !h.contains(&x) {
                h.push(x);
            }
        }
        h
    });
    let dialer = Dialer {
        company: company.to_string(),
        hosts: hosts.clone(),
        allow_private: entry.allow_private,
        timeout: entry.timeout,
        pace: Arc::clone(&entry.pace),
    };
    let off_host = |field: &str, url: &str| {
        let host = host_of(url).unwrap_or_else(|| url.to_string());
        Refusal {
            field: Some(field.to_string()),
            host: Some(host.clone()),
            ..Refusal::new(
                "endpoint",
                format!(
                    "company {company}: {field} {url} is on {host}, which is not one of the entry's hosts (services.{company}.hosts)"
                ),
            )
        }
    };
    if dialer.admits(&issuer).is_err() {
        return Err(off_host("auth.issuer", &issuer));
    }
    let meta_url =
        crate::auth::http::insert_well_known(&issuer, wire::AS_METADATA_SUFFIX).unwrap_or_default();
    let meta = dialer
        .document(&meta_url)
        .map_err(|r| Refusal::from_http(company, r))?;
    let m = &meta.json;
    if m["issuer"].as_str() != Some(issuer.as_str()) {
        return Err(Refusal::new(
            "issuer",
            format!(
                "company {company}: the metadata's issuer {} is not auth.issuer {issuer:?}, byte for byte",
                m["issuer"]
            ),
        ));
    }
    let listed = m[wire::DOMAINS_MEMBER].as_array().is_some_and(|d| {
        d.iter()
            .filter_map(Value::as_str)
            .any(|x| x.eq_ignore_ascii_case(&domain))
    });
    if !listed {
        return Err(Refusal::new(
            "domains",
            format!(
                "company {company}: the metadata's {} does not list {domain}",
                wire::DOMAINS_MEMBER
            ),
        ));
    }
    let es256 = m[wire::DPOP_ALGS_MEMBER]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x == wire::ES256));
    if !es256 {
        let detail = if m.get(wire::PREFIXED_DPOP_ALGS_MEMBER).is_some() {
            format!(
                "company {company}: the DPoP algorithms are published under {}, not {}",
                wire::PREFIXED_DPOP_ALGS_MEMBER,
                wire::DPOP_ALGS_MEMBER
            )
        } else {
            format!(
                "company {company}: {} does not include {}",
                wire::DPOP_ALGS_MEMBER,
                wire::ES256
            )
        };
        return Err(Refusal::new("es256", detail));
    }
    let endpoint = |name: &str, required: bool| -> Result<Option<String>, Refusal> {
        match m[name].as_str() {
            Some(u) => Ok(Some(u.to_string())),
            None if required => Err(Refusal::new(
                "metadata",
                format!("company {company}: the metadata names no {name}"),
            )),
            None => Ok(None),
        }
    };
    let token_endpoint = endpoint("token_endpoint", true)?.unwrap_or_default();
    let revocation_endpoint = endpoint("revocation_endpoint", true)?.unwrap_or_default();
    let authorization_endpoint = endpoint(
        "authorization_endpoint",
        poppy.pointer("/auth/direct").is_some(),
    )?;
    let device_authorization_endpoint = endpoint(
        "device_authorization_endpoint",
        poppy.pointer("/auth/device").is_some(),
    )?;
    // Every URL the two documents name, and the host rule over each.
    let mut named: Vec<(String, String)> = vec![
        ("token_endpoint".into(), token_endpoint.clone()),
        ("revocation_endpoint".into(), revocation_endpoint.clone()),
    ];
    for (f, u) in [
        ("authorization_endpoint", &authorization_endpoint),
        (
            "device_authorization_endpoint",
            &device_authorization_endpoint,
        ),
    ] {
        if let Some(u) = u {
            named.push((f.into(), u.clone()));
        }
    }
    if let Some(u) = string(poppy, "/auth/mediated/endpoint") {
        named.push(("auth.mediated.endpoint".into(), u.to_string()));
    }
    let listed = |v: &Value, at: &str| Listed {
        url: v[at].as_str().unwrap_or_default().to_string(),
        resource: v["resource"].as_str().map(str::to_string),
    };
    let mut mcp_urls = Vec::new();
    let mut openapi = Vec::new();
    for (i, a) in poppy["apis"].as_array().into_iter().flatten().enumerate() {
        let api = listed(a, "url");
        if a["type"] == wire::API_MCP {
            mcp_urls.push(api.url.clone());
        } else if a["type"] == wire::API_OPENAPI {
            openapi.push(api.clone());
        }
        if let Some(r) = &api.resource {
            named.push((format!("apis[{i}].resource"), r.clone()));
        }
        named.push((format!("apis[{i}].url"), api.url));
    }
    let mut conversation = None;
    for (i, p) in poppy
        .pointer("/agent/protocols")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let proto = listed(p, "endpoint");
        if let Some(r) = &proto.resource {
            named.push((format!("agent.protocols[{i}].resource"), r.clone()));
        }
        named.push((format!("agent.protocols[{i}].endpoint"), proto.url.clone()));
        if conversation.is_none() && p["type"] == wire::CONVERSATION_TYPE {
            conversation = Some(proto);
        }
    }
    if let Some(u) = string(poppy, "/web/browser_session_endpoint") {
        named.push(("web.browser_session_endpoint".into(), u.to_string()));
    }
    let mut operations = None;
    for (name, ext) in poppy["extensions"].as_object().into_iter().flatten() {
        if let Some(u) = ext["endpoint"].as_str() {
            named.push((format!("extensions.{name}.endpoint"), u.to_string()));
        }
        if let Some(r) = ext["resource"].as_str() {
            named.push((format!("extensions.{name}.resource"), r.to_string()));
        }
        // An extension of another major version is one agentd does not
        // speak, and is ignored (§3.3).
        if name == wire::operations::EXTENSION
            && ext["version"] == wire::operations::VERSION
            && ext["endpoint"].is_string()
        {
            operations = Some(listed(ext, "endpoint"));
        }
    }
    for (field, url) in &named {
        if dialer.admits(url).is_err() {
            return Err(off_host(field, url));
        }
    }
    let scopes = |ptr: &str| {
        poppy.pointer(ptr).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
    };
    let custom_scopes = poppy
        .pointer("/auth/custom_scopes")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|d| (k.clone(), d.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let max_age = lifetime(match (doc.max_age, meta.max_age) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    });
    Ok((
        Verified {
            company: company.to_string(),
            name: string(poppy, "/organization/name")
                .unwrap_or_default()
                .to_string(),
            domain,
            issuer,
            token_endpoint,
            revocation_endpoint,
            authorization_endpoint,
            device_authorization_endpoint,
            direct_scopes: scopes("/auth/direct/scopes"),
            device_scopes: scopes("/auth/device/scopes"),
            custom_scopes,
            hosts,
            mcp: BTreeMap::new(),
            mcp_urls: mcp_urls.clone(),
            conversation,
            openapi,
            operations,
            interfaces: entry.interfaces.clone(),
            max_age,
            verified_at: Instant::now(),
        },
        mcp_urls,
    ))
}

/// Each of the entry's MCP servers and the `url` it stands for: the one MCP
/// API under the entry's name, or each `mcp_names` names. Several APIs with
/// no `mcp_names`, an API `mcp_names` leaves out, and one it names that the
/// document does not list are each refused with `mcp_names`.
fn name_servers(entry: &Entry, listed: &[String]) -> Result<BTreeMap<String, String>, Refusal> {
    let company = &entry.name;
    let named: Vec<(&String, &String)> = entry
        .servers
        .iter()
        .filter_map(|(server, url)| url.as_ref().map(|u| (u, server)))
        .collect();
    if named.is_empty() {
        return match listed {
            [] => Err(Refusal::new(
                "mcp",
                format!("company {company}: the discovery document lists no MCP API"),
            )),
            [one] => Ok(entry
                .servers
                .iter()
                .map(|(server, _)| (server.clone(), one.clone()))
                .collect()),
            several => Err(Refusal::new(
                "mcp_names",
                format!(
                    "company {company}: the discovery document lists {} MCP APIs ({}) — name each with services.{company}.mcp_names",
                    several.len(),
                    several.join(", ")
                ),
            )),
        };
    }
    let unnamed: Vec<&String> = listed
        .iter()
        .filter(|u| !named.iter().any(|(n, _)| *n == *u))
        .collect();
    let unlisted: Vec<&String> = named
        .iter()
        .map(|(u, _)| *u)
        .filter(|u| !listed.contains(u))
        .collect();
    if !unnamed.is_empty() || !unlisted.is_empty() {
        let mut why = Vec::new();
        if !unnamed.is_empty() {
            why.push(format!(
                "services.{company}.mcp_names names no server for {}",
                unnamed
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !unlisted.is_empty() {
            why.push(format!(
                "the discovery document lists no MCP API at {}",
                unlisted
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        return Err(Refusal::new(
            "mcp_names",
            format!("company {company}: {}", why.join("; ")),
        ));
    }
    Ok(named
        .into_iter()
        .map(|(url, server)| (server.clone(), url.clone()))
        .collect())
}

/// The cache: the last verified documents, and whether the Company is
/// suspended.
pub struct Discovery {
    pub entry: Entry,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    verified: Option<Arc<Verified>>,
    /// Why the Company is unusable until a restart: its issuer changed.
    suspended: Option<String>,
    /// What was last said, so each outcome is said once per change.
    said: Option<String>,
    stale_said: bool,
    /// The MCP APIs a refetch last listed, when they differ from the ones
    /// verified first: said once per change.
    apis_said: Option<Vec<String>>,
    /// Why the last attempt failed, until one succeeds: its reason and
    /// line.
    failed: Option<(String, String)>,
}

/// What a refetch changed that is said, for the caller to log.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Verified(Value),
    Refused(Refusal),
    Stale {
        age_s: u64,
        error: String,
    },
    IssuerChanged {
        old: String,
        new: String,
    },
    ApisChanged {
        added: Vec<String>,
        removed: Vec<String>,
    },
}

impl Discovery {
    pub fn new(entry: Entry) -> Discovery {
        Discovery {
            entry,
            state: Mutex::new(State::default()),
        }
    }

    /// The documents as last verified, without fetching anything: what a
    /// caller that must not wait on the network reads — the issuer a call's
    /// content came from, the hosts a URL is held to. `None` before the
    /// first verification and while the Company is suspended.
    pub fn last(&self) -> Option<Arc<Verified>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.suspended.is_some() {
            return None;
        }
        st.verified.clone()
    }

    /// Why the last verification failed — its reason and line — until one
    /// succeeds.
    pub fn failed(&self) -> Option<(String, String)> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed
            .clone()
    }

    /// Why the Company is unusable until a restart, when it is.
    pub fn suspended(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .suspended
            .clone()
    }

    /// The verified documents, fetched again when they have expired, and
    /// what is to be said about what changed: each note once per change.
    pub fn current(&self) -> (Result<Arc<Verified>, String>, Vec<Note>) {
        self.current_with(&|e: &Entry| verify(e))
    }

    fn current_with(
        &self,
        verify: &dyn Fn(&Entry) -> Verification,
    ) -> (Result<Arc<Verified>, String>, Vec<Note>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut notes = Vec::new();
        if let Some(why) = &st.suspended {
            return (Err(why.clone()), notes);
        }
        if let Some(v) = &st.verified
            && v.verified_at.elapsed() < v.max_age
        {
            return (Ok(Arc::clone(v)), notes);
        }
        let fresh = verify(&self.entry).and_then(|(v, listed)| match &st.verified {
            // A refetch keeps the servers as first verified.
            Some(old) => Ok((v, listed, Some(Arc::clone(old)))),
            None => {
                let mcp = servers_of(&self.entry, &listed)?;
                Ok((Verified { mcp, ..v }, listed, None))
            }
        });
        let out = match fresh {
            Ok((mut v, listed, Some(old))) => {
                if v.issuer != old.issuer {
                    let why = format!(
                        "company {}: its issuer changed from {} to {} — suspended until a restart",
                        self.entry.name, old.issuer, v.issuer
                    );
                    notes.push(Note::IssuerChanged {
                        old: old.issuer.clone(),
                        new: v.issuer.clone(),
                    });
                    st.suspended = Some(why.clone());
                    st.verified = None;
                    return (Err(why), notes);
                }
                let uses_mcp = self.entry.interfaces.contains(&Interface::Mcp);
                if listed == old.mcp_urls || !uses_mcp {
                    st.apis_said = None;
                } else if st.apis_said.as_ref() != Some(&listed) {
                    st.apis_said = Some(listed.clone());
                    notes.push(Note::ApisChanged {
                        added: listed
                            .iter()
                            .filter(|u| !old.mcp_urls.contains(u))
                            .cloned()
                            .collect(),
                        removed: old
                            .mcp_urls
                            .iter()
                            .filter(|u| !listed.contains(u))
                            .cloned()
                            .collect(),
                    });
                }
                v.mcp = old.mcp.clone();
                v.mcp_urls = old.mcp_urls.clone();
                let v = Arc::new(v);
                st.verified = Some(Arc::clone(&v));
                st.stale_said = false;
                Ok(v)
            }
            Ok((v, _, None)) => {
                let v = Arc::new(v);
                st.verified = Some(Arc::clone(&v));
                Ok(v)
            }
            Err(refusal) => match st.verified.clone() {
                Some(old) if old.verified_at.elapsed() < STALE_FOR => {
                    if !st.stale_said {
                        st.stale_said = true;
                        notes.push(Note::Stale {
                            age_s: old.verified_at.elapsed().as_secs(),
                            error: refusal.detail.clone(),
                        });
                    }
                    return (Ok(old), notes);
                }
                _ => {
                    st.verified = None;
                    Err(refusal)
                }
            },
        };
        st.failed = out
            .as_ref()
            .err()
            .map(|r| (r.reason.to_string(), r.detail.clone()));
        let said = match &out {
            Ok(v) => format!("verified {}", v.line()),
            Err(r) => format!("refused {} {}", r.reason, r.detail),
        };
        if st.said.as_deref() != Some(said.as_str()) {
            st.said = Some(said);
            notes.push(match &out {
                Ok(v) => Note::Verified(v.line()),
                Err(r) => Note::Refused(r.clone()),
            });
        }
        (out.map_err(|r| r.detail), notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(servers: &[(&str, Option<&str>)]) -> Entry {
        Entry {
            name: "shop".into(),
            endpoint: "https://example.com".into(),
            hosts: None,
            allow_private: false,
            timeout: Duration::from_secs(1),
            servers: servers
                .iter()
                .map(|(s, u)| (s.to_string(), u.map(str::to_string)))
                .collect(),
            pace: Arc::default(),
            sign_in: Vec::new(),
            scopes: Vec::new(),
            interfaces: vec![Interface::Mcp],
            api_allow: Vec::new(),
            apis: Vec::new(),
        }
    }

    fn urls(u: &[&str]) -> Vec<String> {
        u.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_mcp_api_is_the_entry_and_several_need_their_names() {
        let one = name_servers(&entry(&[("shop", None)]), &urls(&["https://m/mcp"])).unwrap();
        assert_eq!(one["shop"], "https://m/mcp");
        let two = urls(&["https://m/mcp", "https://m/orders"]);
        let e = name_servers(&entry(&[("shop", None)]), &two).unwrap_err();
        assert_eq!(e.reason, "mcp_names");
        assert!(e.detail.contains("https://m/orders"), "{}", e.detail);
        let named = entry(&[
            ("search", Some("https://m/mcp")),
            ("orders", Some("https://m/orders")),
        ]);
        let m = name_servers(&named, &two).unwrap();
        assert_eq!(m["orders"], "https://m/orders");
        // An MCP API the map leaves out, and one it names that is not
        // listed, are each refused, naming the URL.
        let partial = entry(&[("search", Some("https://m/mcp"))]);
        let e = name_servers(&partial, &two).unwrap_err();
        assert_eq!(e.reason, "mcp_names");
        assert!(
            e.detail.contains("names no server for https://m/orders"),
            "{}",
            e.detail
        );
        let extra = entry(&[
            ("search", Some("https://m/mcp")),
            ("gone", Some("https://m/gone")),
        ]);
        let e = name_servers(&extra, &urls(&["https://m/mcp"])).unwrap_err();
        assert!(
            e.detail.contains("lists no MCP API at https://m/gone"),
            "{}",
            e.detail
        );
        assert_eq!(
            name_servers(&entry(&[("shop", None)]), &[])
                .unwrap_err()
                .reason,
            "mcp"
        );
    }

    fn verified(issuer: &str, mcp: &[&str]) -> (Verified, Vec<String>) {
        let v = Verified {
            company: "shop".into(),
            name: "Shop".into(),
            domain: "example.com".into(),
            issuer: issuer.into(),
            token_endpoint: format!("{issuer}/token"),
            revocation_endpoint: format!("{issuer}/revoke"),
            authorization_endpoint: None,
            device_authorization_endpoint: None,
            direct_scopes: None,
            device_scopes: None,
            custom_scopes: BTreeMap::new(),
            hosts: vec!["example.com".into()],
            mcp: BTreeMap::new(),
            mcp_urls: urls(mcp),
            conversation: None,
            openapi: Vec::new(),
            operations: None,
            interfaces: vec![Interface::Mcp],
            max_age: Duration::ZERO,
            verified_at: Instant::now(),
        };
        (v, urls(mcp))
    }

    /// A refetch that changes the MCP APIs says so and keeps the servers as
    /// they were verified; one that names another issuer suspends the
    /// Company; one that fails keeps the last verified documents, saying
    /// they are stale, once.
    #[test]
    fn a_refetch_changes_no_server_and_an_issuer_change_suspends() {
        let d = Discovery::new(entry(&[("shop", None)]));
        let (v, notes) =
            d.current_with(&|_| Ok(verified("https://as.example", &["https://m/mcp"])));
        assert_eq!(v.unwrap().mcp["shop"], "https://m/mcp");
        assert!(matches!(notes.as_slice(), [Note::Verified(_)]), "{notes:?}");
        let (v, notes) = d.current_with(&|_| {
            Ok(verified(
                "https://as.example",
                &["https://m/mcp", "https://m/new"],
            ))
        });
        let v = v.unwrap();
        assert_eq!(v.mcp.len(), 1, "the servers stay as verified");
        assert_eq!(v.mcp["shop"], "https://m/mcp");
        assert_eq!(
            notes,
            vec![Note::ApisChanged {
                added: urls(&["https://m/new"]),
                removed: Vec::new()
            }]
        );
        // Said once per change: the same list again says nothing, another
        // one is said.
        let (_, notes) = d.current_with(&|_| {
            Ok(verified(
                "https://as.example",
                &["https://m/mcp", "https://m/new"],
            ))
        });
        assert!(notes.is_empty(), "said once: {notes:?}");
        let (_, notes) = d.current_with(&|_| Ok(verified("https://as.example", &["https://m/b"])));
        assert_eq!(
            notes,
            vec![Note::ApisChanged {
                added: urls(&["https://m/b"]),
                removed: urls(&["https://m/mcp"])
            }]
        );
        let (v, notes) = d.current_with(&|_| Err(Refusal::new("fetch", "down")));
        assert!(v.is_ok(), "the last verified documents serve");
        assert!(
            matches!(notes.as_slice(), [Note::Stale { .. }]),
            "{notes:?}"
        );
        let (_, notes) = d.current_with(&|_| Err(Refusal::new("fetch", "down")));
        assert!(notes.is_empty(), "said once: {notes:?}");
        let (v, notes) =
            d.current_with(&|_| Ok(verified("https://other.example", &["https://m/mcp"])));
        assert!(v.unwrap_err().contains("suspended until a restart"));
        assert!(
            matches!(notes.as_slice(), [Note::IssuerChanged { .. }]),
            "{notes:?}"
        );
        let (v, _) = d.current_with(&|_| Ok(verified("https://as.example", &["https://m/mcp"])));
        assert!(v.is_err(), "suspended stays suspended");
    }

    #[test]
    fn a_lifetime_is_clamped() {
        if std::env::var_os("AGENTD_TEST_COMPANY_MAX_AGE_MS").is_some() {
            return;
        }
        assert_eq!(lifetime(None), DEFAULT_MAX_AGE);
        assert_eq!(lifetime(Some(Duration::from_secs(1))), MIN_MAX_AGE);
        assert_eq!(lifetime(Some(Duration::from_secs(1 << 30))), MAX_MAX_AGE);
        assert_eq!(
            lifetime(Some(Duration::from_secs(600))),
            Duration::from_secs(600)
        );
    }
}
