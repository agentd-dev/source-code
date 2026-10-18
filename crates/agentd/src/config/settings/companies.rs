// SPDX-License-Identifier: AGPL-3.0-only
//! Acting at Companies for a person (RFC 0047 §5.3): `security.companies` —
//! the one User this instance acts for, its client identity, the seal key —
//! and the fields a `services` entry of `kind: company` adds. Both are
//! parsed whatever the build; without the `companies` feature they are
//! refused by name.
//!
//! A company entry declares a Company by the domain whose discovery document
//! agentd reads. The Company's MCP APIs become MCP servers here, at load:
//! one named after the entry when the Company lists one MCP API, or one per
//! `mcp_names` value when it lists several. Their names are known before any
//! document is read, so the tag fold, the tool grants and every name a
//! workflow uses see them as they see any server; where each one lives is
//! learned at discovery, and only the supervisor ever dials one.

use super::{Diagnostics, McpServer, Secret, ServiceKind, Settings, StoreKind};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The default User: every operator principal counts as the one User.
pub const OPERATOR: &str = "operator";
/// Where the client signing key lives when `client.key_file` names none.
pub const DEFAULT_KEY_FILE: &str = "/var/lib/agentd/company-client-key";
/// The path of the default key set under the `client_id`'s origin.
pub const DEFAULT_JWKS_PATH: &str = "/companies/jwks.json";
/// Concurrent Company calls per Company when `limits.in_flight` says none.
pub const DEFAULT_IN_FLIGHT: u32 = 4;

/// `security.companies`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Companies {
    /// The User this instance acts for at every Company: `operator` (the
    /// default) or one principal id.
    pub user: Option<String>,
    /// The client identity every Company sees.
    pub client: Option<Client>,
    /// 32 random bytes, as a secret reference: when set, the User ID records
    /// are sealed with it, so a store that holds them cannot read them.
    pub seal_key: Option<Secret>,
    /// Browser MCP servers to keep off every Company's hosts.
    pub browsers: Vec<String>,
    pub limits: Limits,
}

impl Companies {
    /// The User every Company sees this instance act for.
    pub fn user(&self) -> &str {
        self.user.as_deref().unwrap_or(OPERATOR)
    }

    pub fn in_flight(&self) -> u32 {
        self.limits.in_flight.unwrap_or(DEFAULT_IN_FLIGHT)
    }
}

/// `security.companies.client`: the client metadata document's contents and
/// the keys behind it (draft §4.1).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Client {
    /// The `client_id`: the HTTPS URL of the client metadata document.
    pub id: String,
    /// `client_name`, shown on Companies' consent pages.
    pub name: String,
    #[serde(default)]
    pub logo_uri: Option<String>,
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// The ES256 signing key (PKCS#8), created `0600` when absent.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Keys whose public halves are published but never signed with: the
    /// next key before a rotation, the previous one after it.
    #[serde(default)]
    pub published_key_files: Vec<String>,
    /// Serve the document and the key set on the A2A listener.
    #[serde(default)]
    pub serve: bool,
}

impl Client {
    /// The key set's URL: as configured, or `/companies/jwks.json` on the
    /// `client_id`'s origin.
    pub fn jwks_uri(&self) -> String {
        match &self.jwks_uri {
            Some(u) => u.clone(),
            None => format!(
                "{}{DEFAULT_JWKS_PATH}",
                origin(&self.id).unwrap_or_default()
            ),
        }
    }

    pub fn key_file(&self) -> &str {
        self.key_file.as_deref().unwrap_or(DEFAULT_KEY_FILE)
    }
}

/// `security.companies.limits`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Concurrent Company calls per Company.
    pub in_flight: Option<u32>,
}

/// `services.<name>.privacy`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    #[default]
    Strict,
    Standard,
}

/// One interface a Company lists that agentd may use.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    Mcp,
    Agent,
    Openapi,
}

/// What makes a synthesized MCP server a Company's: the entry it belongs to
/// and, when the entry names several MCP APIs, the `url` it stands for.
/// Never configured; set by [`synthesize`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompanyServer {
    pub entry: String,
    pub url: Option<String>,
}

/// The tags a Company channel carries when its entry declares none: what
/// arrives is Company-authored text, and a call can change state at the
/// Company (§6.3).
pub fn default_tags() -> BTreeMap<String, Vec<String>> {
    BTreeMap::from([(
        "*".to_string(),
        vec!["untrusted_input".to_string(), "egress".to_string()],
    )])
}

/// `scheme://authority` of `url`, as written.
pub fn origin(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    (end > 0).then(|| &url[..url.len() - rest.len() + end])
}

/// The host of `url`, lowercased, without its port.
pub fn host_of(url: &str) -> Option<String> {
    let o = origin(url)?;
    let authority = &o[o.find("://")? + 3..];
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if authority.starts_with('[') {
        authority.split(']').next().map(|h| format!("{h}]"))?
    } else {
        authority
            .rsplit_once(':')
            .filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .map_or(authority, |(h, _)| h)
            .to_string()
    };
    Some(host.to_ascii_lowercase())
}

/// Whether `url` is an HTTPS URL with a host — the pattern the draft's kit
/// writes as `^https://[^\s/?#]+`.
pub fn is_https_url(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| !c.is_whitespace() && !matches!(c, '/' | '?' | '#'))
}

/// Whether `host` matches the host glob `pattern`: `example.com` itself, or
/// with `*.example.com` any name under it. Hosts compare ignoring ASCII case
/// (draft §3.1).
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let (p, h) = (pattern.to_ascii_lowercase(), host.to_ascii_lowercase());
    match p.strip_prefix("*.") {
        Some(base) => h.len() > base.len() + 1 && h.ends_with(&format!(".{base}")),
        None => p == h,
    }
}

/// The MCP servers a company entry stands for, by name, with the `url` each
/// one is when `mcp_names` names them.
pub fn server_names(name: &str, svc: &super::Service) -> Vec<(String, Option<String>)> {
    if !svc
        .interfaces
        .as_deref()
        .unwrap_or(&[Interface::Mcp])
        .contains(&Interface::Mcp)
    {
        return Vec::new();
    }
    if svc.mcp_names.is_empty() {
        return vec![(name.to_string(), None)];
    }
    svc.mcp_names
        .iter()
        .map(|(url, server)| (server.clone(), Some(url.clone())))
        .collect()
}

/// Add the MCP servers of every company entry to `s.mcp.servers`: named as
/// [`server_names`] says, `ns` their own name so a Company tool can never
/// take a bare name, the entry's tags (or [`default_tags`]), `allow`,
/// `exclude`, `rate` and `timeout`, no AAuth, and no endpoint — the
/// supervisor learns that at discovery. A name another server already has
/// is refused, and the server is left out.
pub(super) fn synthesize(s: &mut Settings, errs: &mut Vec<String>) {
    let mut added: Vec<McpServer> = Vec::new();
    for (entry, svc) in &s.services {
        if svc.kind != ServiceKind::Company {
            continue;
        }
        for (server, url) in server_names(entry, svc) {
            let taken = s
                .mcp
                .servers
                .iter()
                .chain(added.iter())
                .find(|x| x.name == server);
            if let Some(other) = taken {
                errs.push(match &other.company {
                    Some(c) => format!(
                        "services.{entry}: its MCP server {server:?} has the name company entry '{}' gives one of its own — name them apart with mcp_names",
                        c.entry
                    ),
                    None => format!(
                        "services.{entry}: its MCP server {server:?} has the name of mcp server '{server}' — rename one (mcp_names names a Company's servers)"
                    ),
                });
                continue;
            }
            let tags = if svc.tags.is_empty() {
                default_tags()
            } else {
                svc.tags.clone()
            };
            added.push(McpServer {
                name: server.clone(),
                endpoint: String::new(),
                service: Some(entry.clone()),
                service_rate: svc.rate.clone(),
                ns: Some(server.clone()),
                headers: BTreeMap::new(),
                tags,
                allow: svc.allow.clone(),
                exclude: svc.exclude.clone(),
                aauth: Some(false),
                oauth: None,
                auth: None,
                timeout: svc.timeout,
                events: Vec::new(),
                event_grants: Vec::new(),
                company: Some(CompanyServer {
                    entry: entry.clone(),
                    url,
                }),
            });
        }
    }
    s.mcp.servers.extend(added);
}

/// Whether `s` declares any Company.
pub fn any_company(s: &Settings) -> bool {
    s.services.values().any(|v| v.kind == ServiceKind::Company)
}

/// The names of the MCP servers that are a Company's.
pub fn company_servers(s: &Settings) -> Vec<&str> {
    s.mcp
        .servers
        .iter()
        .filter(|x| x.company.is_some())
        .map(|x| x.name.as_str())
        .collect()
}

/// Whether `server` is one of a Company's MCP servers.
pub fn is_company_server(s: &Settings, server: &str) -> bool {
    s.mcp
        .servers
        .iter()
        .any(|x| x.name == server && x.company.is_some())
}

/// Why a workflow may not name a Company's MCP server itself — in an
/// `mcp.tool` or `mcp.resource` step, a resource wait or a `subscribe`
/// start: a Company is reached only through its tools, by their own names,
/// where the User-ownership rule, the in-flight limit and the typed errors
/// apply (RFC 0047 §5.6).
pub fn direct_use_refused(server: &str) -> String {
    format!(
        "mcp server {server:?} is a Company's: call its tools with a `tool` step (`{server}.<tool>`), which acts for the Company's User; a workflow reaches a Company no other way"
    )
}

/// The steps of `w`, at any depth, that name a Company's MCP server
/// themselves ([`direct_use_refused`]). One check for a configured
/// definition at load and for one the runtime reads, stores or the agent
/// writes; a templated server name is refused when its step runs.
pub fn workflow_refusals(s: &Settings, w: &crate::engine::model::Workflow) -> Vec<String> {
    fn walk(
        s: &Settings,
        wf: &str,
        steps: &BTreeMap<String, crate::engine::model::Step>,
        out: &mut Vec<String>,
    ) {
        for (id, st) in steps {
            let direct = match st.kind.as_str() {
                "mcp.tool" | "mcp.resource" | "subscribe" => true,
                "wait" => st.field_str("on") == Some("resource"),
                _ => false,
            };
            if direct
                && let Some(srv) = st.field_str("server")
                && is_company_server(s, srv)
            {
                out.push(format!(
                    "workflow {wf:?} step {id:?}: {}",
                    direct_use_refused(srv)
                ));
            }
            if let Some(b) = &st.body {
                walk(s, wf, &b.steps, out);
            }
            for b in st.branches.values() {
                walk(s, wf, &b.steps, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(s, &w.name, &w.steps, &mut out);
    out
}

/// Every check RFC 0047 §5.3 makes at load, on `security.companies` and the
/// company entries.
pub(super) fn validate(s: &Settings, d: &mut Diagnostics) {
    let mut err = |m: String| d.errors.push(m);
    let c = &s.security.companies;
    let declared = c != &Companies::default();
    let entries: Vec<(&String, &super::Service)> = s
        .services
        .iter()
        .filter(|(_, v)| v.kind == ServiceKind::Company)
        .collect();
    // Every other kind keeps the vocabulary it had.
    for (name, svc) in s
        .services
        .iter()
        .filter(|(_, v)| v.kind != ServiceKind::Company)
    {
        for (set, what) in [
            (svc.hosts.is_some(), "hosts"),
            (svc.allow_private.is_some(), "allow_private"),
            (svc.interfaces.is_some(), "interfaces"),
            (!svc.mcp_names.is_empty(), "mcp_names"),
            (svc.privacy.is_some(), "privacy"),
        ] {
            if set {
                err(format!(
                    "services.{name}: `{what}` applies to `kind: company` entries only (this entry is `kind: {}`)",
                    svc.kind.as_str()
                ));
            }
        }
    }
    if !cfg!(feature = "companies") {
        if declared {
            err("security.companies needs the `companies` feature (build with --features companies)".into());
        }
        for (name, _) in &entries {
            err(format!(
                "services.{name}: `kind: company` needs the `companies` feature (build with --features companies)"
            ));
        }
        return;
    }
    if entries.is_empty() && !declared {
        return;
    }
    // The User.
    let user = c.user();
    if user != OPERATOR && !is_principal_id(user) {
        err(format!(
            "security.companies.user: {user:?} is neither `operator` nor a principal id (`<role>:<id>`)"
        ));
    }
    let operator_rules = s
        .a2a
        .principals
        .iter()
        .filter(|p| p.role == super::Role::Operator)
        .count();
    if user == OPERATOR && operator_rules > 1 {
        err(format!(
            "security.companies.user is `operator`, and a2a.principals holds {operator_rules} rules of role operator — every one would act as the one User at every Company; name the User's principal id"
        ));
    }
    // The client identity.
    match &c.client {
        None if !entries.is_empty() => err(
            "security.companies.client is required with a `kind: company` entry: it is who agentd is to every Company (id, name)".into(),
        ),
        None => {}
        Some(cl) => validate_client(s, cl, &mut err),
    }
    if let Some(l) = c.limits.in_flight
        && l == 0
    {
        err(
            "security.companies.limits.in_flight: 0 would let no Company call run; at least 1"
                .into(),
        );
    }
    let company_names = company_servers(s);
    for b in &c.browsers {
        if company_names.contains(&b.as_str()) {
            err(format!(
                "security.companies.browsers: {b:?} is a Company's MCP server, not a browser"
            ));
        } else if !s.mcp.servers.iter().any(|x| x.name == *b) {
            err(format!(
                "security.companies.browsers: {b:?} names no configured mcp server"
            ));
        }
    }
    if let Some(Secret(v)) = &c.seal_key
        && let Some(e) = super::unresolved_secret_ref(v)
    {
        err(format!("security.companies.seal_key: {e}"));
    }
    if entries.is_empty() {
        return;
    }
    // A User ID must be stable (§4.2), and a remote store must never hold
    // one in plaintext.
    match s.store.kind {
        StoreKind::None | StoreKind::Memory => err(format!(
            "store.kind {} forgets every User ID when the process ends, and a Company must see the same User ID across Sessions (draft §4.2) — use store.kind file, mcp or http",
            if s.store.kind == StoreKind::None { "none" } else { "memory" }
        )),
        StoreKind::Mcp | StoreKind::Http if c.seal_key.is_none() => err(
            "a `kind: company` entry with a remote store (store.kind mcp or http) needs security.companies.seal_key, so the store holds the User IDs sealed".into(),
        ),
        _ => {}
    }
    for (name, svc) in &entries {
        validate_entry(name, svc, &mut err);
    }
    // What else names a server must not name a Company's: its tools are
    // reached by their own names, through the supervisor alone.
    let named: Vec<(String, &str)> = [
        s.knowledge
            .server
            .as_deref()
            .map(|n| ("knowledge.server".to_string(), n)),
        s.search
            .server
            .as_deref()
            .map(|n| ("search.server".to_string(), n)),
    ]
    .into_iter()
    .flatten()
    .chain(
        s.skills
            .sources
            .iter()
            .map(|src| ("skills.sources[].server".to_string(), src.server.as_str())),
    )
    .chain(
        s.tools
            .overrides
            .iter()
            .map(|(t, ov)| (format!("tools.overrides.{t}"), ov.server.as_str())),
    )
    .chain(
        s.store
            .mcp
            .iter()
            .map(|m| ("store.mcp.server".to_string(), m.server.as_str())),
    )
    .collect();
    for (at, server) in named {
        if company_names.contains(&server) {
            err(format!(
                "{at}: {server:?} is a Company's MCP server, whose tools are reached only by their own names"
            ));
        }
    }
}

fn validate_client(s: &Settings, cl: &Client, err: &mut impl FnMut(String)) {
    if !is_https_url(&cl.id) {
        err(format!(
            "security.companies.client.id: {:?} is not an HTTPS URL; a client_id is the HTTPS URL of its metadata document (draft §4.1)",
            cl.id
        ));
        return;
    }
    if cl.name.trim().is_empty() {
        err("security.companies.client.name: a client_name is required".into());
    }
    if let Some(l) = &cl.logo_uri
        && !is_https_url(l)
    {
        err(format!(
            "security.companies.client.logo_uri: {l:?} is not an HTTPS URL"
        ));
    }
    let home = host_of(&cl.id);
    let jwks = cl.jwks_uri();
    if !is_https_url(&jwks) {
        err(format!(
            "security.companies.client.jwks_uri: {jwks:?} is not an HTTPS URL"
        ));
    } else if host_of(&jwks) != home {
        err(format!(
            "security.companies.client.jwks_uri: {jwks:?} is not on the client_id's host ({}); the draft has every published URL on its domain (§4.1)",
            home.unwrap_or_default()
        ));
    }
    if cl.serve {
        if !cfg!(feature = "a2a") {
            err("security.companies.client.serve needs the `a2a` feature: the documents are served on the A2A listener".into());
        }
        match (&s.a2a.listen, &s.a2a.url) {
            (None, _) => err("security.companies.client.serve needs a2a.listen: the documents are served on the A2A listener".into()),
            (_, None) => err("security.companies.client.serve needs a2a.url, the origin Companies reach the listener at; it must be the client_id's".into()),
            (_, Some(url)) if origin(url).map(|o| o.trim_end_matches('/')) != origin(&cl.id) => err(format!(
                "security.companies.client.serve: a2a.url {url:?} is not the client_id's origin ({}); the listener serves the client_id's own URL",
                origin(&cl.id).unwrap_or_default()
            )),
            _ => {}
        }
        // Served beside the listener's own routes, so on paths of their own.
        let (a, b) = (path_of(&cl.id), path_of(&jwks));
        for p in [&a, &b] {
            if p == "/" || p.starts_with("/.well-known/") || p.starts_with("/oauth2/") {
                err(format!(
                    "security.companies.client.serve: {p} is a path the A2A listener answers itself; publish the client metadata and key set elsewhere"
                ));
            }
        }
        if a == b {
            err("security.companies.client.serve: the client_id and jwks_uri are one URL".into());
        }
    }
}

/// The path (and query) of `url`, `/` when it has none.
pub fn path_of(url: &str) -> String {
    match origin(url) {
        Some(o) if url.len() > o.len() => url[o.len()..].to_string(),
        _ => "/".to_string(),
    }
}

fn validate_entry(name: &str, svc: &super::Service, err: &mut impl FnMut(String)) {
    let at = format!("services.{name}");
    // The protocol defines a Company's credential (§4.1–§4.3).
    for (set, what) in [
        (svc.auth.is_some(), "auth"),
        (!svc.headers.is_empty(), "headers"),
        (svc.methods.is_some(), "methods"),
        (svc.breaker.is_some(), "breaker"),
    ] {
        if set {
            err(format!(
                "{at}: `{what}` does not apply to a `kind: company` entry — the protocol defines how agentd authenticates there"
            ));
        }
    }
    // A domain, not a document: the discovery document's place is the
    // protocol's (§3).
    match svc.endpoint.strip_prefix("https://") {
        Some(rest) => {
            let rest = rest.strip_suffix('/').unwrap_or(rest);
            if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
                err(format!(
                    "{at}.endpoint: {:?} — a company endpoint is `https://<host>`, the domain whose discovery document is read; a path is refused",
                    svc.endpoint
                ));
            }
        }
        None => err(format!(
            "{at}.endpoint: {:?} is not an https://<host> URL",
            svc.endpoint
        )),
    }
    for h in svc.hosts.iter().flatten() {
        let base = h.strip_prefix("*.").unwrap_or(h);
        if base.is_empty()
            || base.contains('*')
            || !base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        {
            err(format!(
                "{at}.hosts: {h:?} is not a host or a `*.<host>` glob"
            ));
        }
    }
    let ifaces = svc.interfaces.as_deref().unwrap_or(&[Interface::Mcp]);
    for i in ifaces {
        if *i != Interface::Mcp {
            err(format!(
                "{at}.interfaces: {} is not used by this build — agentd uses a Company's `mcp` APIs",
                match i {
                    Interface::Agent => "agent",
                    _ => "openapi",
                }
            ));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for (url, server) in &svc.mcp_names {
        if !is_https_url(url) {
            err(format!(
                "{at}.mcp_names: {url:?} is not an HTTPS URL, and an MCP API's url is one"
            ));
        }
        if server.is_empty()
            || server == "code"
            || !server
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            err(format!(
                "{at}.mcp_names: {server:?} is not a server name ([a-zA-Z0-9_-]+, not `code`)"
            ));
        }
        if seen.contains(&server.as_str()) {
            err(format!(
                "{at}.mcp_names: two MCP APIs are both named {server:?}"
            ));
        }
        seen.push(server);
    }
}

/// `<role>:<id>`, the shape a principal id has.
fn is_principal_id(id: &str) -> bool {
    id.split_once(':').is_some_and(|(role, rest)| {
        matches!(role, "operator" | "user" | "agent") && !rest.is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration in `yaml` (with the store in a scratch dir), loaded
    /// as `agentd --config` loads it: its settings, or every error.
    fn load(yaml: &str) -> Result<Settings, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        let yaml = yaml.replace("__DIR__", &dir.path().display().to_string());
        std::fs::write(&path, yaml).unwrap();
        super::super::load(&["--config".to_string(), path.display().to_string()], &[])
            .map(|(l, _)| l.settings)
            .map_err(|e| e.to_string())
    }

    const CLIENT: &str = "security:\n  companies:\n    client: {id: \"https://agent.example/companies/client.json\", name: Agent}\n";
    const STORE: &str = "store: {kind: file, file: {path: __DIR__}}\n";

    fn company(extra: &str) -> String {
        format!(
            "{CLIENT}{STORE}services:\n  shop: {{kind: company, endpoint: \"https://example.com\"{extra}}}\n"
        )
    }

    /// A company entry stands for one MCP server named after it: `ns` its own
    /// name, the default tags, no endpoint until discovery, and marked as a
    /// Company's — or one per `mcp_names` value.
    #[cfg(feature = "companies")]
    #[test]
    fn a_company_entry_is_its_mcp_servers() {
        let s = load(&company("")).unwrap();
        let shop = s.mcp.servers.iter().find(|x| x.name == "shop").unwrap();
        assert_eq!(shop.ns.as_deref(), Some("shop"));
        assert_eq!(shop.tags, default_tags());
        assert!(shop.endpoint.is_empty());
        assert_eq!(shop.aauth, Some(false));
        assert_eq!(
            shop.company.as_ref().map(|c| c.entry.as_str()),
            Some("shop")
        );
        assert!(shop.to_spec().unwrap().company.is_some());
        let s = load(&company(
            ", mcp_names: {\"https://example.com/mcp\": search, \"https://example.com/orders\": orders}, tags: {\"*\": [untrusted_input, egress, sensitive]}",
        ));
        // `sensitive` with untrusted input and egress is the trifecta.
        assert!(s.unwrap_err().contains("lethal-trifecta"));
        let s = load(&company(
            ", mcp_names: {\"https://example.com/mcp\": search, \"https://example.com/orders\": orders}",
        ))
        .unwrap();
        let names: Vec<&str> = company_servers(&s);
        assert_eq!(names, ["search", "orders"], "in the order of their URLs");
    }

    /// Each refusal RFC 0047 §5.3 lists, named.
    #[cfg(feature = "companies")]
    #[test]
    fn what_load_refuses_and_why() {
        let refused = |yaml: &str, why: &str| {
            let e = load(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        refused(
            &company(", mcp_names: {\"https://example.com/a\": x, \"https://example.com/b\": x}"),
            "two MCP APIs are both named \"x\"",
        );
        refused(
            &format!(
                "{}mcp:\n  servers:\n    - {{name: search, endpoint: \"https://other.example/mcp\"}}\n",
                company(", mcp_names: {\"https://example.com/a\": search}")
            ),
            "has the name of mcp server 'search'",
        );
        refused(
            &company("").replace("\"https://example.com\"", "\"https://example.com/shop\""),
            "a path is refused",
        );
        refused(
            &company(", headers: {X-A: b}"),
            "`headers` does not apply to a `kind: company` entry",
        );
        refused(
            &format!(
                "{STORE}services:\n  shop: {{kind: company, endpoint: \"https://example.com\"}}\n"
            ),
            "security.companies.client is required",
        );
        refused(
            &company("").replace(
                "https://agent.example/companies/client.json",
                "http://agent.example/c.json",
            ),
            "is not an HTTPS URL",
        );
        refused(
            &company("").replace(
                "name: Agent}",
                "name: Agent, jwks_uri: \"https://elsewhere.example/jwks.json\"}",
            ),
            "is not on the client_id's host",
        );
        refused(
            &company("").replace("name: Agent}", "name: Agent, serve: true}"),
            "client.serve",
        );
        refused(
            &format!(
                "{}a2a:\n  principals:\n    - {{match: {{sub: a}}, role: operator}}\n    - {{match: {{sub: b}}, role: operator}}\n",
                company("")
            ),
            "holds 2 rules of role operator",
        );
        refused(
            &company("").replace(STORE, "store: {kind: memory}\n"),
            "store.kind memory forgets every User ID",
        );
        refused(
            &company(", interfaces: [mcp, agent]"),
            "interfaces: agent is not used",
        );
        refused(
            &company(", hosts: [\"https://example.com\"]"),
            "is not a host or a `*.<host>` glob",
        );
        refused(
            &format!(
                "{CLIENT}{STORE}services:\n  m: {{kind: mcp, endpoint: \"https://m.example/mcp\", hosts: [m.example]}}\n"
            ),
            "`hosts` applies to `kind: company` entries only",
        );
        refused(
            &format!(
                "{}mcp:\n  servers:\n    - {{name: x, service: shop}}\n",
                company("")
            ),
            "which is `kind: company`",
        );
        refused(
            &format!("{}knowledge: {{server: shop}}\n", company("")),
            "knowledge.server",
        );
        refused(
            &format!(
                "{}store: {{kind: mcp, mcp: {{server: shop}}}}\n",
                company("").replace(STORE, "")
            ),
            "store.mcp.server: \"shop\" is a Company's MCP server",
        );
        // A workflow names a Company's server only through its tools, at any
        // depth; a templated name is refused when its step runs.
        let flow = |steps: &str, last: &str| {
            format!(
                "{}workflows:\n  - name: w\n    steps:\n{steps}      f: {{kind: finish, depends_on: [{last}], status: completed}}\n",
                company("")
            )
        };
        for (steps, last) in [
            (
                "      s: {kind: once}\n      t: {kind: mcp.tool, server: shop, tool: search, depends_on: [s]}\n",
                "t",
            ),
            (
                "      s: {kind: once}\n      t: {kind: mcp.resource, server: shop, op: read, uri: \"x://y\", depends_on: [s]}\n",
                "t",
            ),
            (
                "      s: {kind: once}\n      t: {kind: wait, on: resource, server: shop, uri: \"x://y\", depends_on: [s]}\n",
                "t",
            ),
            (
                "      t: {kind: subscribe, server: shop, uri: \"x://y\"}\n",
                "t",
            ),
            (
                "      s: {kind: once}\n      p: {kind: parallel, depends_on: [s], branches: {a: {steps: {t: {kind: mcp.tool, server: shop, tool: search}}}}}\n",
                "p",
            ),
        ] {
            refused(
                &flow(steps, last),
                "step \"t\": mcp server \"shop\" is a Company's: call its tools with a `tool` step (`shop.<tool>`)",
            );
        }
        for fine in [
            "      s: {kind: once}\n      t: {kind: tool, name: shop.search, args: {query: q}, depends_on: [s]}\n",
            "      s: {kind: once, inputs: {srv: shop}}\n      t: {kind: mcp.tool, server: \"{{ inputs.srv }}\", tool: search, depends_on: [s]}\n",
        ] {
            load(&flow(fine, "t")).unwrap_or_else(|e| panic!("{fine}: {e}"));
        }
        refused(
            &company("").replace("name: Agent}", "name: Agent}\n    browsers: [web]"),
            "names no configured mcp server",
        );
        refused(
            &company("").replace("name: Agent}", "name: Agent}\n    seal_key: \"0101\""),
            "security.companies.seal_key carries an inline credential",
        );
        refused(
            &company("").replace("name: Agent}", "name: Agent}\n    user: dana"),
            "neither `operator` nor a principal id",
        );
        // And a remote store needs the seal key.
        let remote = company("").replace(
            STORE,
            "store: {kind: http, http: {base_url: \"https://store.example\"}}\n",
        );
        refused(&remote, "needs security.companies.seal_key");
    }

    #[cfg(not(feature = "companies"))]
    #[test]
    fn a_company_needs_the_feature() {
        let e = load(&company("")).unwrap_err();
        assert!(
            e.contains("`kind: company` needs the `companies` feature"),
            "{e}"
        );
    }

    #[test]
    fn hosts_match_exactly_or_under_a_glob() {
        assert!(host_matches("example.com", "Example.COM"));
        assert!(!host_matches("example.com", "www.example.com"));
        assert!(host_matches("*.example.com", "www.example.com"));
        assert!(host_matches("*.example.com", "a.b.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("*.example.com", "badexample.com"));
        assert_eq!(
            host_of("https://Auth.Example.com:8443/x").as_deref(),
            Some("auth.example.com")
        );
        assert_eq!(
            host_of("https://127.0.0.1:9/").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(origin("https://a.example/x/y?z"), Some("https://a.example"));
        assert!(is_https_url("https://a"));
        assert!(!is_https_url("https:///a"));
        assert!(!is_https_url("http://a"));
    }
}
