// SPDX-License-Identifier: AGPL-3.0-only
//! Acting at Companies for a person (RFC 0047 §5.3): `security.companies` —
//! the one User this instance acts for, its client identity, the seal key —
//! and the fields a `services` entry of `kind: company` adds, among them the
//! sign-in types agentd may start with the User and the scopes it may ask
//! for. Both are parsed whatever the build; without the `companies` feature
//! they are refused by name.
//!
//! A company entry declares a Company by the domain whose discovery document
//! agentd reads. The Company's MCP APIs become MCP servers here, at load:
//! one named after the entry when the Company lists one MCP API, or one per
//! `mcp_names` value when it lists several. So do its OpenAPI APIs, for the
//! operations `api_allow` lists: each such operation is a tool of its own,
//! named by a fixed rule from its `api_allow` entry ([`operation_name`]),
//! under a server named after the entry or one per `api_names` value. Their
//! names are known before any document is read, so the tag fold, the tool
//! grants and every name a workflow uses see them as they see any server;
//! where each one lives is learned at discovery, and only the supervisor
//! ever dials one.

use super::{Diagnostics, Dur, McpServer, Secret, ServiceKind, Settings, StoreKind};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The default User: every operator principal counts as the one User.
pub const OPERATOR: &str = "operator";
/// Where the client signing key lives when `client.key_file` names none.
pub const DEFAULT_KEY_FILE: &str = "/var/lib/agentd/company-client-key";
/// The path of the default key set under the `client_id`'s origin.
pub const DEFAULT_JWKS_PATH: &str = "/companies/jwks.json";
/// The path Direct Sign-In returns the User to under the `client_id`'s
/// origin, when `client.redirect_uri` names none.
pub const DEFAULT_REDIRECT_PATH: &str = "/companies/callback";
/// Said at load wherever a Company is configured: the draft moves without a
/// version signal, so agentd follows one commit of it (rfcs/0047 §11).
pub const DRAFT_WARNING: &str = "Acting at Companies tracks the Personal Agent Protocol 0.1 at 95fcbf2, a draft: its documents, messages and flows may change between releases";
/// Concurrent Company calls per Company when `limits.in_flight` says none.
pub const DEFAULT_IN_FLIGHT: u32 = 4;
/// Open conversations, each with its reader, per instance when
/// `limits.conversations` says none.
pub const DEFAULT_CONVERSATIONS: u32 = 16;

/// `security.companies`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Companies {
    /// The User this instance acts for at every Company: `operator` (the
    /// default) or one principal id.
    pub user: Option<String>,
    /// The client identity every Company sees.
    pub client: Option<Client>,
    /// 32 random bytes, as a secret reference: the Account Tokens are sealed
    /// with it, and when it is set the User ID records too, so a store that
    /// holds them cannot read them. `key_custody: local` only.
    pub seal_key: Option<Secret>,
    /// Who holds the seal key: the supervisor, read from `seal_key`
    /// (`local`, the default), or the external signer `signer` names
    /// (`external`), which wraps each record's data key and derives the
    /// record-id keys, so agentd holds neither.
    pub key_custody: super::delegation::KeyCustody,
    /// The external signer, under `key_custody: external`.
    pub signer: Option<Signer>,
    /// What the messages agentd sends in a Company's conversations tell it
    /// of the User's situation (draft §7.4): nothing it does not set.
    pub context: Option<Context>,
    /// Browser MCP servers to keep off every Company's hosts.
    pub browsers: Vec<String>,
    pub limits: Limits,
    /// The standing permissions the operator gives agentd to confirm an
    /// operation without asking the User (RFC 0047 §5.8.1): restart-only,
    /// never written by anything but this configuration.
    pub standing: Vec<Standing>,
}

/// `security.companies.signer`: the external signer that holds the seal key
/// (RFC 0044 §7.P2b, `docs/external-signer.md`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    /// The signer's unix socket, an absolute path.
    pub socket: String,
    /// The uid the signer process runs as: the socket's peer must be it.
    pub uid: u32,
    /// The bound on one request; default 5s.
    #[serde(default)]
    pub timeout: Option<Dur>,
    /// The signer's key-encryption key each record's data key is wrapped
    /// under.
    pub seal_key: String,
    /// The signer's HMAC-SHA256 key the record-id keys are derived under.
    pub derive_key: String,
}

impl Signer {
    /// The signer client this names.
    #[cfg(feature = "oauth")]
    pub fn signer(&self) -> crate::sec::signer::Signer {
        crate::sec::signer::Signer::new(&self.socket, self.uid, self.timeout.map(|d| d.0))
    }
}

impl Companies {
    /// The signer that holds the seal key, under `key_custody: external`.
    pub fn external(&self) -> Option<&Signer> {
        match self.key_custody {
            super::delegation::KeyCustody::External => self.signer.as_ref(),
            super::delegation::KeyCustody::Local => None,
        }
    }

    /// Whether the records are sealed: a seal key, wherever it is held.
    pub fn sealed(&self) -> bool {
        self.seal_key.is_some() || self.external().is_some()
    }
}

/// An extension agentd lists in its client metadata document (draft §3.3,
/// §4.1).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Extension {
    /// The Operations extension, version 1: a Company proposes an action and
    /// agentd confirms it only with the User's approval of its terms.
    Operations,
}

impl Extension {
    pub fn as_str(self) -> &'static str {
        match self {
            Extension::Operations => "operations",
        }
    }
}

/// One standing permission (`security.companies.standing[]`): an
/// operation proposed at `company` through `via`, whose every term key is in
/// `terms` and for which `when` holds, is confirmed with `approved_by:
/// standing_permission` until `expires`, at most `uses` times.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Standing {
    /// The company entry.
    pub company: String,
    /// The channel that proposed the operation: the draft gives an
    /// operation no kind of action, so the channel stands for it.
    pub via: Via,
    /// Every term key it covers: a revision with a key outside them, or with
    /// no `terms`, asks the User.
    pub terms: Vec<String>,
    /// A CEL predicate over `terms` and `summary`.
    pub when: String,
    /// When it stops applying, RFC 3339.
    pub expires: String,
    /// How many confirms it covers; none limits it to `expires` alone.
    #[serde(default)]
    pub uses: Option<u32>,
    /// The labels of the User's accounts at the Company it covers; none
    /// covers the account labelled [`DEFAULT_ACCOUNT`] alone — and the
    /// signed-out Session — so a second account is never covered unlisted.
    #[serde(default)]
    pub accounts: Option<Vec<String>>,
}

/// The label of the account a sign-in names none for (RFC 0047 §9, P5b).
pub const DEFAULT_ACCOUNT: &str = "default";

/// Whether `label` is one a User's account at a Company may be named by: 1
/// to 32 of `a`–`z`, `0`–`9`, `_` and `-`. agentd's own word, never sent to
/// the Company.
pub fn is_account_label(label: &str) -> bool {
    (1..=32).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// A standing permission's channel: one of the Company's MCP tools, by a
/// glob of its registered name, or one OpenAPI operation.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Via {
    #[serde(default)]
    pub tool: Option<String>,
    /// `METHOD /path-template`.
    #[serde(default)]
    pub api: Option<String>,
}

/// Whether the client metadata document lists the Operations extension:
/// agentd recognises operations, and offers the `company.operation.*`
/// tools, only then.
pub fn lists_operations(s: &Settings) -> bool {
    s.security
        .companies
        .client
        .as_ref()
        .is_some_and(|c| c.extensions.contains(&Extension::Operations))
}

/// Whether a company entry has the stream `company.<name>`: its
/// conversations' events go there when it uses `agent`, and its operations'
/// final states when the Operations extension is listed.
pub fn has_stream(s: &Settings, svc: &super::Service) -> bool {
    interfaces_of(svc).contains(&Interface::Agent) || lists_operations(s)
}

impl Companies {
    /// The User every Company sees this instance act for.
    pub fn user(&self) -> &str {
        self.user.as_deref().unwrap_or(OPERATOR)
    }

    pub fn in_flight(&self) -> u32 {
        self.limits.in_flight.unwrap_or(DEFAULT_IN_FLIGHT)
    }

    pub fn conversations(&self) -> u32 {
        self.limits.conversations.unwrap_or(DEFAULT_CONVERSATIONS)
    }
}

/// `security.companies.context`, and a company entry's own `context`: the
/// facts about the User's situation a conversation message may carry, each
/// in the draft's format (§7.4). What is not set is not shared.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Context {
    /// A language tag (RFC 5646), such as `en-US`.
    pub locale: Option<String>,
    /// An IANA time zone name, such as `America/Los_Angeles`.
    pub time_zone: Option<String>,
    /// Share `user_available`: whether the User has a client attached to
    /// agentd now.
    pub share_availability: Option<bool>,
}

impl Context {
    /// This context, each field it leaves unset taken from `base`.
    pub fn over(&self, base: &Context) -> Context {
        Context {
            locale: self.locale.clone().or_else(|| base.locale.clone()),
            time_zone: self.time_zone.clone().or_else(|| base.time_zone.clone()),
            share_availability: self.share_availability.or(base.share_availability),
        }
    }

    /// Whether it shares anything.
    pub fn is_empty(&self) -> bool {
        self.locale.is_none() && self.time_zone.is_none() && self.share_availability != Some(true)
    }
}

/// The context a company entry's messages carry: its own, over
/// `security.companies.context`.
pub fn context_of(s: &Settings, svc: &super::Service) -> Context {
    let base = s.security.companies.context.clone().unwrap_or_default();
    match &svc.context {
        Some(c) => c.over(&base),
        None => base,
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
    /// Where Direct Sign-In returns the User (draft §4.5).
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Serve the document and the key set on the A2A listener — and, when a
    /// Company lists `direct`, the Direct Sign-In callback.
    #[serde(default)]
    pub serve: bool,
    /// The extensions the document lists.
    #[serde(default)]
    pub extensions: Vec<Extension>,
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

    /// Where Direct Sign-In returns the User: as configured, or
    /// `/companies/callback` on the `client_id`'s origin.
    pub fn redirect_uri(&self) -> String {
        match &self.redirect_uri {
            Some(u) => u.clone(),
            None => format!(
                "{}{DEFAULT_REDIRECT_PATH}",
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
    /// Open conversations, each followed by a reader, per instance.
    pub conversations: Option<u32>,
}

/// `services.<name>.privacy`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    #[default]
    Strict,
    Standard,
}

/// A sign-in type (`services.<name>.sign_in`, draft §4.4).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SignIn {
    /// Device Sign-In (§4.6): the User opens a link on any device and checks
    /// a code there.
    Device,
    /// Direct Sign-In (§4.5): the User signs in on the Company's page and is
    /// sent back to the callback agentd serves.
    Direct,
    /// Mediated Sign-In (§4.7): read only so it is refused by name.
    Mediated,
}

impl SignIn {
    pub fn as_str(self) -> &'static str {
        match self {
            SignIn::Device => "device",
            SignIn::Direct => "direct",
            SignIn::Mediated => "mediated",
        }
    }
}

/// The sign-in types `svc` lets agentd start, in its order.
pub fn sign_in_of(svc: &super::Service) -> Vec<SignIn> {
    svc.sign_in.clone().unwrap_or_default()
}

/// Whether any Company may be signed in to directly: the callback is then
/// served, and listed in the client metadata document's `redirect_uris`.
pub fn signs_in_directly(s: &Settings) -> bool {
    s.services
        .values()
        .any(|v| v.kind == ServiceKind::Company && sign_in_of(v).contains(&SignIn::Direct))
}

/// Whether any Company lets agentd sign the User in.
pub fn signs_in(s: &Settings) -> bool {
    s.services
        .values()
        .any(|v| v.kind == ServiceKind::Company && !sign_in_of(v).is_empty())
}

/// One interface a Company lists that agentd may use.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    /// Its MCP servers (draft §6).
    Mcp,
    /// Its Company Agent, in conversations (§7).
    Agent,
    /// Its OpenAPI APIs (§6).
    Openapi,
}

impl Interface {
    pub fn as_str(self) -> &'static str {
        match self {
            Interface::Mcp => "mcp",
            Interface::Agent => "agent",
            Interface::Openapi => "openapi",
        }
    }
}

/// The interfaces a company entry uses: its own, or `[mcp]`.
pub fn interfaces_of(svc: &super::Service) -> Vec<Interface> {
    svc.interfaces
        .clone()
        .unwrap_or_else(|| vec![Interface::Mcp])
}

/// Whether some company entry uses `interface`.
pub fn any_uses(s: &Settings, interface: Interface) -> bool {
    s.services
        .values()
        .any(|v| v.kind == ServiceKind::Company && interfaces_of(v).contains(&interface))
}

/// The `company.*` tools that reach a Company's agent in a conversation.
pub const CONVERSATION_TOOLS: [&str; 4] = [
    "company.converse",
    "company.read",
    "company.handoff",
    "company.close",
];
/// The `company.*` tools that reach a Company's OpenAPI APIs.
pub const API_TOOLS: [&str; 2] = ["company.api", "company.api.describe"];
/// The `company.*` tools that read, confirm and cancel a Company's
/// operations (RFC 0047 §5.8.1).
pub const OPERATION_TOOLS: [&str; 3] = [
    "company.operation.get",
    "company.operation.confirm",
    "company.operation.cancel",
];
/// The tool that asks the User whether to talk with a Company directly
/// (§5.8.2).
pub const DIRECT_TOOL: &str = "company.direct";

/// Whether `tool` reaches a Company's agent in a conversation.
pub fn is_conversation_tool(tool: &str) -> bool {
    CONVERSATION_TOOLS.contains(&tool)
}

/// Whether `tool` reads, confirms or cancels a Company's operation.
pub fn is_operation_tool(tool: &str) -> bool {
    OPERATION_TOOLS.contains(&tool)
}

/// Whether `tool` reaches a Company's agent, its OpenAPI APIs or its
/// operations: a tool that carries the Company's content, and acts at the
/// Company its `company` argument names.
pub fn is_interface_tool(tool: &str) -> bool {
    is_conversation_tool(tool)
        || API_TOOLS.contains(&tool)
        || is_operation_tool(tool)
        || tool == DIRECT_TOOL
}

/// The stream a Company's conversation events are appended to.
pub fn stream_of(entry: &str) -> String {
    format!("company.{entry}")
}

/// The tags the `company.*` tools that reach a Company's conversations,
/// APIs and operations carry: those of every entry that uses `agent` or
/// `openapi`, and of every entry once the Operations extension is listed —
/// its own, or [`default_tags`] — as its synthesized MCP servers carry them
/// (§6.3).
pub fn interface_tags(s: &Settings) -> Vec<crate::sec::scope::TrifectaTag> {
    let mut out = Vec::new();
    let operations = lists_operations(s);
    for svc in s.services.values().filter(|v| {
        v.kind == ServiceKind::Company
            && (operations
                || interfaces_of(v)
                    .iter()
                    .any(|i| matches!(i, Interface::Agent | Interface::Openapi)))
    }) {
        for t in entry_tags(svc) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

/// The tags every channel to a company entry carries: its own, or
/// [`default_tags`].
pub fn entry_tags(svc: &super::Service) -> Vec<crate::sec::scope::TrifectaTag> {
    let tags = if svc.tags.is_empty() {
        default_tags()
    } else {
        svc.tags.clone()
    };
    let mut out = Vec::new();
    for t in tags
        .values()
        .flatten()
        .filter_map(|t| crate::sec::scope::TrifectaTag::parse(t))
    {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// What makes a synthesized MCP server a Company's: the entry it belongs to,
/// the interface it stands for — one of the Company's MCP APIs, or one of its
/// OpenAPI APIs, whose tools are the operations `api_allow` lists — and,
/// when `mcp_names` or `api_names` names it, the `url` it stands for. Never
/// configured; set by [`synthesize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyServer {
    pub entry: String,
    pub url: Option<String>,
    pub interface: Interface,
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

/// The host of `url`, lowercased, without its port — none unless the
/// authority is a plain `host[:port]`: a DNS name or IP address, with no
/// userinfo and nothing else. A browser ends the authority at `\` as at `/`,
/// so `https://other.example\@a.example/` is at `other.example` there and at
/// `a.example` to a reader that does not; a URL whose host two readers could
/// differ on names none.
pub fn host_of(url: &str) -> Option<String> {
    let o = origin(url)?;
    let authority = &o[o.find("://")? + 3..];
    // The host, the port after it, and whether the host is a plain one: an
    // IPv6 literal in brackets, else a name or IPv4 address.
    let (host, port, plain) = match authority.strip_prefix('[') {
        Some(rest) => {
            let end = rest.find(']')?;
            let literal = &rest[..end];
            let plain = !literal.is_empty()
                && literal
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'));
            (&authority[..end + 2], &authority[end + 2..], plain)
        }
        None => {
            let end = authority.find(':').unwrap_or(authority.len());
            let name = &authority[..end];
            let plain = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
            (name, &authority[end..], plain)
        }
    };
    let port = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    (plain && port).then(|| host.to_ascii_lowercase())
}

/// Whether `url` is an HTTPS URL with a plain host ([`host_of`]) — the
/// pattern the draft's kit writes as `^https://[^\s/?#]+`, held to a host
/// every reader of the URL agrees on.
pub fn is_https_url(url: &str) -> bool {
    url.starts_with("https://") && host_of(url).is_some()
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
    if !interfaces_of(svc).contains(&Interface::Mcp) {
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

/// The servers a company entry's operation tools are named under, by name,
/// with the OpenAPI API `url` each one is when `api_names` names them: one
/// named after the entry — `<entry>_api` when the entry's own MCP server has
/// that name — or one per `api_names` value; none unless the entry uses
/// `openapi` and `api_allow` lists an operation.
pub fn api_server_names(name: &str, svc: &super::Service) -> Vec<(String, Option<String>)> {
    if !interfaces_of(svc).contains(&Interface::Openapi)
        || svc.api_allow.as_ref().is_none_or(Vec::is_empty)
    {
        return Vec::new();
    }
    if svc.api_names.is_empty() {
        let taken = server_names(name, svc).iter().any(|(n, _)| n == name);
        return vec![(
            if taken {
                format!("{name}_api")
            } else {
                name.to_string()
            },
            None,
        )];
    }
    svc.api_names
        .iter()
        .map(|(url, server)| (server.clone(), Some(url.clone())))
        .collect()
}

/// The longest name an operation tool has within its server.
pub const MAX_OPERATION_NAME: usize = 64;

/// The name, within its server, of the tool for the `api_allow` entry `op`
/// (`METHOD /path-template`): in lower case, each run of characters other
/// than `a-z` and `0-9` one `_`, none at either end, at most
/// [`MAX_OPERATION_NAME`] characters — `GET /orders/{order_id}` is
/// `get_orders_order_id`. The operator's words, never the Company's: a
/// Company that renames an `operationId` renames no tool.
pub fn operation_name(op: &str) -> String {
    let mut out = String::new();
    for c in op.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out.truncate(MAX_OPERATION_NAME);
    out.trim_end_matches('_').to_string()
}

/// Each operation `svc`'s `api_allow` lists, as its tool's name within its
/// server ([`operation_name`]) and the entry as written, in its order.
pub fn operations_of(svc: &super::Service) -> Vec<(String, String)> {
    svc.api_allow
        .iter()
        .flatten()
        .map(|op| (operation_name(op), op.clone()))
        .collect()
}

/// The tags the operation tool registered as `tool` (`<server>.<name>`)
/// carries, from its entry's `tags` (or [`default_tags`]): when a key other
/// than `*` names it — its name, or a glob matching it — those of each key
/// that matches it, `*` included; otherwise every tag the entry lists, as
/// `company.api` and the entry's MCP tools carry them. A key refines only
/// the tools it names, so one written for another of the entry's servers
/// takes nothing from the operation tools.
pub fn operation_tags(
    tags: &BTreeMap<String, Vec<String>>,
    tool: &str,
) -> Vec<crate::sec::scope::TrifectaTag> {
    let named = tags
        .keys()
        .any(|key| key.trim() != "*" && crate::registry::pattern_matches(key, tool));
    let mut out = Vec::new();
    for t in tags
        .iter()
        .filter(|(key, _)| !named || crate::registry::pattern_matches(key, tool))
        .flat_map(|(_, t)| t)
        .filter_map(|t| crate::sec::scope::TrifectaTag::parse(t))
    {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Whether `server` is one of a Company's OpenAPI APIs, whose tools are the
/// operations its entry's `api_allow` lists.
pub fn is_api_server(s: &Settings, server: &str) -> bool {
    s.mcp.servers.iter().any(|x| {
        x.name == server
            && x.company
                .as_ref()
                .is_some_and(|c| c.interface == Interface::Openapi)
    })
}

/// The registered names of the operation tools of the Company OpenAPI API
/// `server`, each `<server>.<name>`; none when `server` is no such API.
pub fn api_tool_names(s: &Settings, server: &str) -> Vec<String> {
    s.mcp
        .servers
        .iter()
        .find(|x| x.name == server)
        .and_then(|x| x.company.as_ref())
        .filter(|c| c.interface == Interface::Openapi)
        .and_then(|c| s.services.get(&c.entry))
        .map(|svc| {
            operations_of(svc)
                .into_iter()
                .map(|(n, _)| format!("{server}.{n}"))
                .collect()
        })
        .unwrap_or_default()
}

/// Add the MCP servers of every company entry to `s.mcp.servers`: named as
/// [`server_names`] says, `ns` their own name so a Company tool can never
/// take a bare name, the entry's tags (or [`default_tags`]), `allow`,
/// `exclude`, `rate` and `timeout`, no AAuth, and no endpoint — the
/// supervisor learns that at discovery. Then the servers its operation tools
/// are named under, as [`api_server_names`] names them, with the entry's
/// tags and `timeout` alone: `api_allow` is their ceiling. A name another
/// server already has is refused, and the server is left out.
///
/// An entry that uses `agent`, or any entry once the Operations extension is
/// listed, gets the stream its conversation events and its operations' final
/// states are appended to, `company.<name>`, with the default retention
/// unless `streams:` declares it.
pub(super) fn synthesize(s: &mut Settings, errs: &mut Vec<String>) {
    let mut added: Vec<McpServer> = Vec::new();
    let mut streams = Vec::new();
    for (entry, svc) in &s.services {
        if svc.kind == ServiceKind::Company && has_stream(s, svc) {
            streams.push(stream_of(entry));
        }
    }
    for stream in streams {
        s.streams.entry(stream).or_default();
    }
    for (entry, svc) in &s.services {
        if svc.kind != ServiceKind::Company {
            continue;
        }
        let servers = server_names(entry, svc)
            .into_iter()
            .map(|(n, u)| (n, u, Interface::Mcp));
        let apis = api_server_names(entry, svc)
            .into_iter()
            .map(|(n, u)| (n, u, Interface::Openapi));
        for (server, url, interface) in servers.chain(apis) {
            let mcp = interface == Interface::Mcp;
            let (what, names) = if mcp {
                ("its MCP server", "mcp_names")
            } else {
                (
                    "the server its operation tools are named under",
                    "api_names",
                )
            };
            let taken = s
                .mcp
                .servers
                .iter()
                .chain(added.iter())
                .find(|x| x.name == server);
            if let Some(other) = taken {
                errs.push(match &other.company {
                    Some(c) if c.entry == *entry => format!(
                        "services.{entry}: {what} {server:?} has the name of another of its own servers — name them apart with api_names or mcp_names"
                    ),
                    Some(c) => format!(
                        "services.{entry}: {what} {server:?} has the name company entry '{}' gives one of its own — name them apart with {names}",
                        c.entry
                    ),
                    None => format!(
                        "services.{entry}: {what} {server:?} has the name of mcp server '{server}' — rename one ({names} names a Company's servers)"
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
                service_rate: svc.rate.clone().filter(|_| mcp),
                ns: Some(server.clone()),
                headers: BTreeMap::new(),
                tags,
                allow: svc.allow.clone().filter(|_| mcp),
                exclude: if mcp { svc.exclude.clone() } else { Vec::new() },
                aauth: Some(false),
                oauth: None,
                auth: None,
                timeout: svc.timeout,
                // A Company's server is dialled through the User's Session,
                // which declares no Tasks extension.
                tasks: crate::config::TasksMode::Off,
                events: Vec::new(),
                event_grants: Vec::new(),
                company: Some(CompanyServer {
                    entry: entry.clone(),
                    url,
                    interface,
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
            // An operation tool's name is the configuration's, known before
            // the Company is read.
            if st.kind == "tool"
                && let Some(tool) = st.field_str("name")
                && !tool.contains("{{")
                && let Some(why) = unknown_operation_tool(s, tool)
            {
                out.push(format!("workflow {wf:?} step {id:?}: {why}"));
            }
            // An `http` step that calls a Company's API names an entry that
            // uses `openapi`; a templated name is held to it when it runs.
            if st.kind == "http"
                && let Some(name) = st
                    .field("auth")
                    .and_then(|a| a.get("company"))
                    .and_then(serde_json::Value::as_str)
                && !name.contains("{{")
                && !s.services.get(name).is_some_and(|v| {
                    v.kind == ServiceKind::Company && interfaces_of(v).contains(&Interface::Openapi)
                })
            {
                out.push(format!(
                    "workflow {wf:?} step {id:?}: auth.company {name:?} is not a company entry that uses `openapi` (services.<name>.interfaces)"
                ));
            }
            // The User's account it acts with, by label; a templated one is
            // held to the label's form when it runs.
            if st.kind == "http"
                && let Some(account) = st
                    .field("auth")
                    .and_then(|a| a.get("account"))
                    .and_then(serde_json::Value::as_str)
                && !account.contains("{{")
                && !is_account_label(account)
            {
                out.push(format!(
                    "workflow {wf:?} step {id:?}: auth.account {account:?} is not an account label (1 to 32 of a-z, 0-9, _ and -)"
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

/// Why the tool name or glob `pattern` is refused, when it names the server
/// a Company's operation tools are named under (`<server>.…`) and none of
/// them: their names are the operator's, known at load
/// ([`operation_name`]).
pub fn unknown_operation_tool(s: &Settings, pattern: &str) -> Option<String> {
    s.mcp.servers.iter().find_map(|x| {
        let c = x.company.as_ref()?;
        if c.interface != Interface::Openapi || !pattern.starts_with(&format!("{}.", x.name)) {
            return None;
        }
        let tools = api_tool_names(s, &x.name);
        (!tools
            .iter()
            .any(|t| crate::registry::pattern_matches(pattern, t)))
        .then(|| {
            format!(
                "{pattern:?} names no operation tool of company entry {}: each is named after an operation services.{}.api_allow lists ({})",
                c.entry,
                c.entry,
                tools.join(", ")
            )
        })
    })
}

/// Who holds the seal key: under `key_custody: external` the signer, named
/// by an absolute socket, a uid, its key-encryption and derivation keys —
/// and no `seal_key` beside it; under `local`, no signer.
fn validate_custody(c: &Companies, d: &mut Diagnostics) {
    use super::delegation::KeyCustody;
    match (c.key_custody, &c.signer) {
        (KeyCustody::External, None) => d.errors.push(
            "security.companies.key_custody external needs security.companies.signer {socket, uid, seal_key, derive_key}".into(),
        ),
        (KeyCustody::Local, Some(_)) => d.errors.push(
            "security.companies.signer is read under key_custody: external alone — say key_custody: external, or drop the signer".into(),
        ),
        (KeyCustody::External, Some(sg)) => {
            if !std::path::Path::new(&sg.socket).is_absolute() {
                d.errors.push(format!(
                    "security.companies.signer.socket {:?} is not an absolute path to the signer's unix socket",
                    sg.socket
                ));
            }
            // SAFETY: geteuid has no failure mode.
            if sg.uid == unsafe { libc::geteuid() } {
                d.warnings.push(format!(
                    "security.companies.signer.uid {} is agentd's own: the peer check cannot tell the signer from a process of agentd's uid, which can also read a same-uid signer's memory and key files — run the signer as another uid",
                    sg.uid
                ));
            }
            if let Some(t) = sg.timeout
                && (t.0.is_zero() || t.0 > std::time::Duration::from_secs(60))
            {
                d.errors
                    .push("security.companies.signer.timeout: between 1ms and 60s".into());
            }
            if sg.seal_key.trim().is_empty() || sg.derive_key.trim().is_empty() {
                d.errors.push(
                    "security.companies.signer names the signer's key-encryption key (seal_key) and its derivation key (derive_key)".into(),
                );
            }
            if c.seal_key.is_some() {
                d.errors.push(
                    "security.companies.seal_key: under key_custody external the signer holds the seal key — name its key-encryption key as security.companies.signer.seal_key, and give no key".into(),
                );
            }
        }
        (KeyCustody::Local, None) => {}
    }
}

/// Every check RFC 0047 §5.3 makes at load, on `security.companies` and the
/// company entries.
pub(super) fn validate(s: &Settings, d: &mut Diagnostics) {
    validate_custody(&s.security.companies, d);
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
            (!svc.api_names.is_empty(), "api_names"),
            (svc.privacy.is_some(), "privacy"),
            (svc.sign_in.is_some(), "sign_in"),
            (svc.scopes.is_some(), "scopes"),
            (svc.api_allow.is_some(), "api_allow"),
            (svc.context.is_some(), "context"),
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
    d.warnings.push(DRAFT_WARNING.into());
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
    if let Some(l) = c.limits.conversations
        && l == 0
    {
        err(
            "security.companies.limits.conversations: 0 would let no conversation open; at least 1"
                .into(),
        );
    }
    if let Some(ctx) = &c.context {
        validate_context("security.companies.context", ctx, &mut err);
        if !any_uses(s, Interface::Agent) {
            err(
                "security.companies.context: no company entry uses `agent` (services.<name>.interfaces), so no conversation would carry it".into(),
            );
        }
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
    for (i, st) in c.standing.iter().enumerate() {
        validate_standing(
            s,
            &format!("security.companies.standing[{i}]"),
            st,
            &mut err,
        );
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
        StoreKind::Mcp | StoreKind::Http if !c.sealed() => err(
            "a `kind: company` entry with a remote store (store.kind mcp or http) needs security.companies.seal_key (or key_custody: external), so the store holds the User IDs sealed".into(),
        ),
        _ => {}
    }
    for (name, svc) in &entries {
        validate_entry(name, svc, &mut err);
    }
    // Signing in keeps an Account Token, which is only ever kept sealed
    // (§4.8: "MUST store Account Tokens as secrets"), and Direct Sign-In
    // returns the User to a callback only a served identity has.
    let signing_in: Vec<&str> = entries
        .iter()
        .filter(|(_, v)| !sign_in_of(v).is_empty())
        .map(|(n, _)| n.as_str())
        .collect();
    if let Some(first) = signing_in.first()
        && !c.sealed()
    {
        err(format!(
            "services.{first}.sign_in needs security.companies.seal_key (or key_custody: external): the Account Token a sign-in gives agentd is kept only sealed with it"
        ));
    }
    let direct = entries
        .iter()
        .find(|(_, v)| sign_in_of(v).contains(&SignIn::Direct));
    if let (Some((name, _)), Some(cl)) = (direct, &c.client)
        && !cl.serve
    {
        err(format!(
            "services.{name}.sign_in: direct returns the User to the callback at security.companies.client.redirect_uri, which agentd serves only with client.serve: true"
        ));
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
    // A rule keyed to an operation tool names one the configuration makes.
    for (i, p) in s.security.policies.iter().enumerate() {
        if let Some(why) = p
            .matcher
            .tool
            .as_deref()
            .and_then(|t| unknown_operation_tool(s, t))
        {
            err(format!("security.policies[{i}].match.tool: {why}"));
        }
    }
}

fn validate_client(s: &Settings, cl: &Client, err: &mut impl FnMut(String)) {
    if let Some(why) = super::client::client_id_url_error(&cl.id) {
        err(format!(
            "security.companies.client.id: {:?} {why}; a client_id is the HTTPS URL of its metadata document (draft §4.1, and the Client ID Metadata Document draft §3 it follows)",
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
    for (i, x) in cl.extensions.iter().enumerate() {
        if cl.extensions[..i].contains(x) {
            err(format!(
                "security.companies.client.extensions: {} is listed twice",
                x.as_str()
            ));
        }
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
            home.clone().unwrap_or_default()
        ));
    }
    let redirect = cl.redirect_uri();
    if !is_https_url(&redirect) {
        err(format!(
            "security.companies.client.redirect_uri: {redirect:?} is not an HTTPS URL"
        ));
    } else if host_of(&redirect) != home {
        err(format!(
            "security.companies.client.redirect_uri: {redirect:?} is not on the client_id's host ({}); the draft has every redirect URI on its domain (§4.1)",
            home.unwrap_or_default()
        ));
    } else if redirect.contains(['?', '#']) {
        err(format!(
            "security.companies.client.redirect_uri: {redirect:?} has a query or a fragment; the Company appends its own query, and must match it exactly (§4.5)"
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
        let served = served_paths(s, cl);
        for p in &served {
            if super::client::listener_answers(p) {
                err(format!(
                    "security.companies.client.serve: {p} is a path the A2A listener answers itself; publish the client metadata, key set and callback elsewhere"
                ));
            }
        }
        for (i, p) in served.iter().enumerate() {
            if served[..i].contains(p) {
                err(format!(
                    "security.companies.client.serve: {p} is the path of two of the client_id, jwks_uri and redirect_uri"
                ));
            }
        }
    }
}

/// The paths the A2A listener serves the client identity at with
/// `client.serve`: the client metadata document, the key set, and the Direct
/// Sign-In callback when a Company may be signed in to directly.
pub fn served_paths(s: &Settings, cl: &Client) -> Vec<String> {
    let mut out = vec![path_of(&cl.id), path_of(&cl.jwks_uri())];
    if signs_in_directly(s) {
        out.push(path_of(&cl.redirect_uri()));
    }
    out
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
    let ifaces = interfaces_of(svc);
    if ifaces.is_empty() {
        err(format!(
            "{at}.interfaces: an empty list uses none of the Company's interfaces; list mcp, agent or openapi"
        ));
    }
    for (i, x) in ifaces.iter().enumerate() {
        if ifaces[..i].contains(x) {
            err(format!("{at}.interfaces: {} is listed twice", x.as_str()));
        }
    }
    // What only one interface reads is refused without it: it would do
    // nothing.
    if !ifaces.contains(&Interface::Mcp) {
        for (set, what) in [
            (svc.rate.is_some(), "rate"),
            (svc.allow.is_some(), "allow"),
            (!svc.exclude.is_empty(), "exclude"),
            (!svc.mcp_names.is_empty(), "mcp_names"),
        ] {
            if set {
                err(format!(
                    "{at}.{what}: it shapes the Company's MCP servers, which the entry does not use — add mcp to {at}.interfaces"
                ));
            }
        }
    }
    if svc.api_allow.is_some() && !ifaces.contains(&Interface::Openapi) {
        err(format!(
            "{at}.api_allow: the OpenAPI operations agentd may call apply when the entry uses `openapi` — add it to {at}.interfaces"
        ));
    }
    for op in svc.api_allow.iter().flatten() {
        if let Err(e) = parse_operation(op) {
            err(format!("{at}.api_allow: {op:?} {e}"));
        }
    }
    if let Some(ctx) = &svc.context {
        if !ifaces.contains(&Interface::Agent) {
            err(format!(
                "{at}.context: a conversation's context applies when the entry uses `agent` — add it to {at}.interfaces"
            ));
        }
        validate_context(&format!("{at}.context"), ctx, err);
    }
    let kinds = sign_in_of(svc);
    if kinds.contains(&SignIn::Mediated) {
        err(format!(
            "{at}.sign_in: mediated is not supported — the User would hand agentd their password through a gate, and a gate's answer is kept in the task's history and given to the model (RFC 0047 decision 5); use device or direct"
        ));
    }
    for (i, k) in kinds.iter().enumerate() {
        if kinds[..i].contains(k) {
            err(format!("{at}.sign_in: {} is listed twice", k.as_str()));
        }
    }
    // RFC 6749 §3.3: a scope token is printable ASCII without a space, a
    // quote or a backslash.
    for s in svc.scopes.iter().flatten() {
        if s.is_empty()
            || !s
                .bytes()
                .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
        {
            err(format!(
                "{at}.scopes: {s:?} is not a scope (printable ASCII, no space, quote or backslash)"
            ));
        }
    }
    if !kinds.is_empty() && svc.scopes.as_ref().is_none_or(Vec::is_empty) {
        err(format!(
            "{at}.sign_in needs {at}.scopes: the most account scopes agentd may ask the User for there, and a sign-in asks for none outside them"
        ));
    }
    if svc.scopes.is_some() && kinds.is_empty() {
        err(format!(
            "{at}.scopes: the scopes agentd may ask for apply when it may sign the User in — list the sign-in types in {at}.sign_in"
        ));
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
    validate_operations(name, svc, err);
}

/// The operation tools a company entry's `api_allow` makes, and the servers
/// `api_names` names them under: each operation listed once, no two named
/// alike, every server name one, and no key of `tags` naming an operation
/// tool the entry has not.
fn validate_operations(name: &str, svc: &super::Service, err: &mut impl FnMut(String)) {
    let at = format!("services.{name}");
    let ops: Vec<(String, String)> = operations_of(svc);
    for (i, (tool, op)) in ops.iter().enumerate() {
        let Ok((method, template)) = parse_operation(op) else {
            continue;
        };
        for (other_tool, other) in &ops[..i] {
            if parse_operation(other)
                .is_ok_and(|(m, t)| m == method && same_template(&t, &template))
            {
                err(format!(
                    "{at}.api_allow: {other:?} and {op:?} name the same operation; list it once"
                ));
            } else if other_tool == tool {
                err(format!(
                    "{at}.api_allow: {other:?} and {op:?} both make the operation tool name {tool:?} (its method and path, in lower case, each run of other characters one `_`) — list one of them"
                ));
            }
        }
    }
    if !svc.api_names.is_empty() {
        if !interfaces_of(svc).contains(&Interface::Openapi) {
            err(format!(
                "{at}.api_names: it names the servers the Company's OpenAPI operation tools are named under, and the entry does not use `openapi` — add it to {at}.interfaces"
            ));
        } else if ops.is_empty() {
            err(format!(
                "{at}.api_names: {at}.api_allow lists no operation, so there is no operation tool to name a server for"
            ));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for (url, server) in &svc.api_names {
        if !is_https_url(url) {
            err(format!(
                "{at}.api_names: {url:?} is not an HTTPS URL, and an OpenAPI API's url is one"
            ));
        }
        if server.is_empty()
            || server == "code"
            || !server
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            err(format!(
                "{at}.api_names: {server:?} is not a server name ([a-zA-Z0-9_-]+, not `code`)"
            ));
        }
        if seen.contains(&server.as_str()) {
            err(format!(
                "{at}.api_names: two OpenAPI APIs are both named {server:?}"
            ));
        }
        seen.push(server);
    }
    // A tag keyed to one of the entry's operation tools names one it has.
    for (server, _) in api_server_names(name, svc) {
        let tools: Vec<String> = ops.iter().map(|(n, _)| format!("{server}.{n}")).collect();
        for key in svc.tags.keys() {
            if key.starts_with(&format!("{server}."))
                && !tools
                    .iter()
                    .any(|t| crate::registry::pattern_matches(key, t))
            {
                err(format!(
                    "{at}.tags: {key:?} names no operation tool of {server:?} — its tools are {}",
                    tools.join(", ")
                ));
            }
        }
    }
}

/// A standing permission (RFC 0047 §5.8.1): spending authority the
/// operator gives agentd at one Company, through one channel, over a set of
/// term keys, held to a CEL predicate and an expiry. One that could never
/// apply is refused, naming why.
fn validate_standing(s: &Settings, at: &str, st: &Standing, err: &mut impl FnMut(String)) {
    if !lists_operations(s) {
        err(format!(
            "{at}: a standing permission confirms operations, which agentd recognises only once security.companies.client.extensions lists operations"
        ));
    }
    let entry = s
        .services
        .get(&st.company)
        .filter(|v| v.kind == ServiceKind::Company);
    let Some(svc) = entry else {
        err(format!(
            "{at}.company: {:?} is not a company entry (services.<name>.kind: company)",
            st.company
        ));
        return;
    };
    let name = &st.company;
    match (&st.via.tool, &st.via.api) {
        (Some(tool), None) => {
            let servers: Vec<String> = server_names(name, svc)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            let api = api_server_names(name, svc).into_iter().find(|(a, _)| {
                operations_of(svc)
                    .iter()
                    .any(|(n, _)| crate::registry::pattern_matches(tool, &format!("{a}.{n}")))
            });
            if let Some((api, _)) = api {
                err(format!(
                    "{at}.via.tool: {tool:?} names operation tools of {api:?}, whose operations are proposed through the Company's API — name the operation with via.api (\"METHOD /path-template\")"
                ));
            } else if servers.is_empty() {
                err(format!(
                    "{at}.via.tool: company {name} uses none of its MCP servers (services.{name}.interfaces lists no mcp)"
                ));
            } else if !servers
                .iter()
                .any(|srv| tool.starts_with(&format!("{srv}.")) && tool.len() > srv.len() + 1)
            {
                err(format!(
                    "{at}.via.tool: {tool:?} names no tool of company {name}'s MCP servers — each of its tools is named <server>.<tool> ({})",
                    servers
                        .iter()
                        .map(|srv| format!("{srv}.*"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        (None, Some(api)) => match parse_operation(api) {
            Err(e) => err(format!("{at}.via.api: {api:?} {e}")),
            Ok((method, template)) => {
                let allowed = svc.api_allow.iter().flatten().any(|a| {
                    parse_operation(a)
                        .is_ok_and(|(m, t)| m == method && same_template(&t, &template))
                });
                if !allowed {
                    err(format!(
                        "{at}.via.api: {api:?} is not in services.{name}.api_allow, so agentd never calls it"
                    ));
                }
            }
        },
        _ => err(format!(
            "{at}.via: name exactly one channel, {{tool: <glob>}} or {{api: \"METHOD /path-template\"}}"
        )),
    }
    if st.terms.is_empty() {
        err(format!(
            "{at}.terms: list every term key it covers; an operation with a key outside them, or with no terms, asks the User"
        ));
    }
    for (i, t) in st.terms.iter().enumerate() {
        if t.trim().is_empty() {
            err(format!("{at}.terms: an empty term key"));
        } else if st.terms[..i].contains(t) {
            err(format!("{at}.terms: {t:?} is listed twice"));
        }
    }
    let expr = st.when.trim().trim_start_matches("CEL:").trim();
    if !cfg!(feature = "cel") {
        err(format!(
            "{at}.when needs the `cel` feature; this build cannot evaluate a standing permission's predicate"
        ));
    } else if let Err(e) = crate::cel::compile_check(expr) {
        err(format!("{at}.when: {e}"));
    }
    if crate::obs::log::rfc3339_ms(&st.expires).is_none() {
        err(format!(
            "{at}.expires: {:?} is not an RFC 3339 time, such as 2026-12-31T00:00:00Z",
            st.expires
        ));
    }
    if st.uses == Some(0) {
        err(format!(
            "{at}.uses: 0 would never apply; at least 1, or leave it out to bound it by expires alone"
        ));
    }
    if let Some(accounts) = &st.accounts {
        if accounts.is_empty() {
            err(format!(
                "{at}.accounts: an empty list covers no account; list the labels it covers, or leave it out to cover {DEFAULT_ACCOUNT:?} alone"
            ));
        }
        for (i, a) in accounts.iter().enumerate() {
            if !is_account_label(a) {
                err(format!(
                    "{at}.accounts: {a:?} is not an account label (1 to 32 of a-z, 0-9, _ and -)"
                ));
            } else if accounts[..i].contains(a) {
                err(format!("{at}.accounts: {a:?} is listed twice"));
            }
        }
    }
}

/// The HTTP methods an OpenAPI operation may have (OpenAPI 3.1 §4.8.9).
pub const OPERATION_METHODS: [&str; 8] = [
    "GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE",
];

/// An `api_allow` entry, `METHOD /path-template`, as its method and its
/// path template.
pub fn parse_operation(op: &str) -> Result<(String, String), String> {
    let (method, path) = op.split_once(' ').ok_or("is not `METHOD /path-template`")?;
    if !OPERATION_METHODS.contains(&method) {
        return Err(format!(
            "names the method {method:?}; an operation's method is one of {}, in capitals",
            OPERATION_METHODS.join(", ")
        ));
    }
    if !path.starts_with('/') || path.contains([' ', '?', '#']) {
        return Err(
            "has a path that does not start with `/`, or carries a space, a query or a fragment"
                .into(),
        );
    }
    let mut open = false;
    for c in path.chars() {
        match c {
            '{' if !open => open = true,
            '}' if open => open = false,
            '{' | '}' => return Err("has a path whose `{…}` parameters do not pair".into()),
            _ => {}
        }
    }
    if open {
        return Err("has a path whose `{…}` parameters do not pair".into());
    }
    Ok((method.to_string(), path.to_string()))
}

/// Whether the path templates `a` and `b` name the same paths: segment by
/// segment equal, each `{…}` parameter matching another whatever its name.
pub fn same_template(a: &str, b: &str) -> bool {
    let param = |s: &str| s.starts_with('{') && s.ends_with('}');
    let (x, y): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
    x.len() == y.len()
        && x.iter()
            .zip(&y)
            .all(|(p, q)| p == q || (param(p) && param(q)))
}

/// A context's fields, each in the format the draft gives it (§7.4).
fn validate_context(at: &str, ctx: &Context, err: &mut impl FnMut(String)) {
    // RFC 5646's shape: a primary subtag of letters, then subtags of
    // letters and digits.
    if let Some(l) = &ctx.locale {
        let mut parts = l.split('-');
        let primary = parts.next().unwrap_or_default();
        let ok = (2..=8).contains(&primary.len())
            && primary.bytes().all(|b| b.is_ascii_alphabetic())
            && parts.all(|p| {
                (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric())
            });
        if !ok {
            err(format!(
                "{at}.locale: {l:?} is not a language tag (RFC 5646), such as en-US"
            ));
        }
    }
    // An IANA time zone name: `Area/Location` segments of letters, digits,
    // `_`, `-` and `+`, or a single one such as `UTC`.
    if let Some(z) = &ctx.time_zone {
        let ok = z.len() <= 64
            && z.split('/').all(|p| {
                !p.is_empty()
                    && p.starts_with(|c: char| c.is_ascii_alphabetic())
                    && p.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+'))
            });
        if !ok {
            err(format!(
                "{at}.time_zone: {z:?} is not an IANA time zone name, such as America/Los_Angeles"
            ));
        }
    }
}

/// `<role>:<id>`, the shape a principal id has.
pub(super) fn is_principal_id(id: &str) -> bool {
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

    /// A configuration that acts at a Company is told at load, once, that
    /// it follows a draft, naming the commit the vendored draft is pinned
    /// to; one that does not is told nothing, so a release binary that
    /// carries the feature and is not asked to use it says what a binary
    /// without it says.
    #[cfg(feature = "companies")]
    #[test]
    fn a_company_loads_and_says_it_tracks_a_draft() {
        let warnings = |yaml: &str| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("c.yaml");
            std::fs::write(
                &path,
                yaml.replace("__DIR__", &dir.path().display().to_string()),
            )
            .unwrap();
            let (l, _) =
                super::super::load(&["--config".to_string(), path.display().to_string()], &[])
                    .unwrap_or_else(|e| panic!("{e}"));
            l.warnings
                .iter()
                .filter(|w| w.contains("Personal Agent Protocol"))
                .count()
        };
        assert_eq!(warnings(&company("")), 1, "a company entry");
        assert_eq!(warnings(CLIENT), 1, "security.companies alone");
        assert_eq!(warnings(STORE), 0, "neither");
        let pin = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/companies/vendor/PIN"),
        )
        .unwrap();
        let commit = pin
            .lines()
            .find_map(|l| l.strip_prefix("commit "))
            .expect("the PIN names its commit");
        assert!(
            DRAFT_WARNING.contains(&commit[..7]),
            "the warning names {DRAFT_WARNING:?}, the vendored draft is {commit}"
        );
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
        refused(&company(", interfaces: []"), "an empty list uses none");
        refused(
            &company(", interfaces: [mcp, agent, mcp]"),
            "interfaces: mcp is listed twice",
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

    /// The seal key is held as its custody says: under `external` by the
    /// signer, named by an absolute socket, its uid and its two keys, with no
    /// `seal_key` beside it — which then stands for one where a sign-in and
    /// a remote store need it — and its socket is among the paths a child is
    /// denied; under `local`, no signer.
    #[cfg(feature = "companies")]
    #[test]
    fn the_seal_key_is_held_as_its_custody_says() {
        let with = |custody: &str| {
            company(", sign_in: [device], scopes: [\"poppy:read\"]")
                .replace("name: Agent}", &format!("name: Agent}}\n{custody}"))
        };
        let signer = "    key_custody: external\n    signer: {socket: /run/signer/s.sock, uid: 991, seal_key: kek, derive_key: ids}\n";
        let s = load(&with(signer)).unwrap();
        assert_eq!(
            s.security
                .companies
                .external()
                .map(|g| g.derive_key.as_str()),
            Some("ids")
        );
        assert!(s.security.companies.sealed());
        assert!(
            s.supervisor_only_paths()
                .iter()
                .any(|p| p.path == std::path::Path::new("/run/signer/s.sock")),
            "the signer's socket is denied to every child"
        );
        // A remote store is sealed through the signer as well.
        let remote = with(signer).replace(
            STORE,
            "store: {kind: mcp, mcp: {server: state}}\nmcp:\n  servers:\n    - {name: state, endpoint: \"https://state.example/mcp\"}\n",
        );
        load(&remote).unwrap();
        let refused = |yaml: &str, why: &str| {
            let e = load(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        refused(
            &with("    key_custody: external\n"),
            "key_custody external needs security.companies.signer",
        );
        refused(
            &with("    signer: {socket: /run/s.sock, uid: 991, seal_key: kek, derive_key: ids}\n"),
            "security.companies.signer is read under key_custody: external alone",
        );
        refused(
            &with(&format!(
                "{signer}    seal_key: \"{{{{secret-file:/run/k}}}}\"\n"
            )),
            "under key_custody external the signer holds the seal key",
        );
        refused(
            &with(
                "    key_custody: external\n    signer: {socket: s.sock, uid: 991, seal_key: kek, derive_key: ids}\n",
            ),
            "is not an absolute path",
        );
        refused(
            &with(
                "    key_custody: external\n    signer: {socket: /s.sock, uid: 991, seal_key: kek, derive_key: \"\"}\n",
            ),
            "its derivation key (derive_key)",
        );
    }

    /// [`load`], with `__DIR__/seal` holding a seal key.
    #[cfg(feature = "companies")]
    fn load_sealed(yaml: &str) -> Result<Settings, String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seal"), "ab".repeat(32)).unwrap();
        let path = dir.path().join("c.yaml");
        let yaml = yaml.replace("__DIR__", &dir.path().display().to_string());
        std::fs::write(&path, yaml).unwrap();
        super::super::load(&["--config".to_string(), path.display().to_string()], &[])
            .map(|(l, _)| l.settings)
            .map_err(|e| e.to_string())
    }

    /// What load refuses about signing in (RFC 0047 §5.3), each named:
    /// Mediated Sign-In with its reason, a sign-in with no seal key for the
    /// Account Token, Direct Sign-In with no callback served, a redirect URI
    /// off the client_id's host, a scope that is not one, and the fields on
    /// an entry of another kind. And what it takes: the redirect URI is the
    /// client_id's `/companies/callback` unless one is named.
    #[cfg(feature = "companies")]
    #[test]
    fn what_load_refuses_about_signing_in_and_why() {
        let sealed = |extra: &str| {
            company(extra).replace(
                "name: Agent}",
                "name: Agent}\n    seal_key: \"{{secret-file:__DIR__/seal}}\"",
            )
        };
        let refused = |yaml: &str, why: &str| {
            let e = load_sealed(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        refused(
            &sealed(", sign_in: [device, mediated]"),
            "services.shop.sign_in: mediated is not supported — the User would hand agentd their password through a gate",
        );
        refused(
            &company(", sign_in: [device], scopes: [\"poppy:read\"]"),
            "services.shop.sign_in needs security.companies.seal_key",
        );
        refused(
            &sealed(", sign_in: [direct], scopes: [\"poppy:read\"]"),
            "direct returns the User to the callback at security.companies.client.redirect_uri, which agentd serves only with client.serve: true",
        );
        refused(
            &sealed(", sign_in: [device, device]"),
            "sign_in: device is listed twice",
        );
        refused(
            &sealed(", sign_in: [device], scopes: [\"poppy:read\", \"a b\"]"),
            "\"a b\" is not a scope",
        );
        refused(
            &sealed(", scopes: [\"poppy:read\"]"),
            "scopes: the scopes agentd may ask for apply when it may sign the User in",
        );
        refused(
            &sealed(", sign_in: [device]"),
            "services.shop.sign_in needs services.shop.scopes",
        );
        refused(
            &sealed(", sign_in: [device], scopes: [\"poppy:read\"]").replace(
                "name: Agent}",
                "name: Agent, redirect_uri: \"https://elsewhere.example/cb\"}",
            ),
            "client.redirect_uri: \"https://elsewhere.example/cb\" is not on the client_id's host",
        );
        refused(
            &format!(
                "{CLIENT}{STORE}services:\n  m: {{kind: mcp, endpoint: \"https://m.example/mcp\", sign_in: [device]}}\n"
            ),
            "`sign_in` applies to `kind: company` entries only",
        );
        let s = load_sealed(&sealed(", sign_in: [device], scopes: [\"poppy:read\"]")).unwrap();
        let cl = s.security.companies.client.as_ref().unwrap();
        assert_eq!(
            cl.redirect_uri(),
            "https://agent.example/companies/callback"
        );
        assert!(signs_in(&s) && !signs_in_directly(&s));
        assert_eq!(sign_in_of(&s.services["shop"]), [SignIn::Device]);
    }

    /// An entry that uses a Company's agent and its APIs: what load takes —
    /// the stream its conversations go to, its operations and its context —
    /// and what it refuses, each named (RFC 0047 §5.3).
    #[cfg(feature = "companies")]
    #[test]
    fn a_company_s_agent_and_apis_are_configured_and_checked() {
        let both = ", interfaces: [agent, openapi], api_allow: [\"GET /orders/{order_id}\", \"POST /orders/{id}/exchanges\"], context: {locale: en-US}";
        let s = load(&company(both).replace(
            "name: Agent}",
            "name: Agent}\n    context: {time_zone: America/Los_Angeles, share_availability: true}\n    limits: {conversations: 2}",
        ))
        .unwrap();
        assert!(
            s.streams.contains_key("company.shop"),
            "its stream is declared"
        );
        assert_eq!(
            company_servers(&s),
            ["shop"],
            "no MCP server without mcp; the server its operation tools are named under"
        );
        assert!(is_api_server(&s, "shop"));
        let ctx = context_of(&s, &s.services["shop"]);
        assert_eq!(ctx.locale.as_deref(), Some("en-US"));
        assert_eq!(
            ctx.time_zone.as_deref(),
            Some("America/Los_Angeles"),
            "inherited"
        );
        assert_eq!(ctx.share_availability, Some(true));
        assert_eq!(s.security.companies.conversations(), 2);
        assert!(any_uses(&s, Interface::Agent) && any_uses(&s, Interface::Openapi));
        assert_eq!(interface_tags(&s).len(), 2, "untrusted_input and egress");
        // An operator's own declaration of the stream is kept.
        let s = load(&format!(
            "{}streams:\n  company.shop: {{retention: {{max_events: 5}}}}\n",
            company(", interfaces: [agent]")
        ))
        .unwrap();
        assert_eq!(s.streams["company.shop"].max_events(), 5);
        let refused = |yaml: &str, why: &str| {
            let e = load(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        refused(
            &company(", api_allow: [\"GET /orders\"]"),
            "services.shop.api_allow: the OpenAPI operations agentd may call apply when the entry uses `openapi`",
        );
        refused(
            &company(", interfaces: [openapi], api_allow: [\"get /orders\"]"),
            "names the method \"get\"",
        );
        refused(
            &company(", interfaces: [openapi], api_allow: [\"GET orders\"]"),
            "does not start with `/`",
        );
        refused(
            &company(", interfaces: [openapi], api_allow: [\"GET /orders/{id\"]"),
            "do not pair",
        );
        refused(
            &company(", context: {locale: en-US}"),
            "services.shop.context: a conversation's context applies when the entry uses `agent`",
        );
        // What shapes only the Company's MCP servers, without them.
        for (field, why) in [
            (
                ", rate: \"30/60s\"",
                "services.shop.rate: it shapes the Company's MCP servers",
            ),
            (", allow: [\"orders_*\"]", "services.shop.allow: it shapes"),
            (
                ", exclude: [\"cancel_*\"]",
                "services.shop.exclude: it shapes",
            ),
            (
                ", mcp_names: {\"https://example.com/mcp\": search}",
                "services.shop.mcp_names: it shapes",
            ),
        ] {
            refused(
                &company(&format!(", interfaces: [agent, openapi]{field}")),
                why,
            );
            load(&company(&format!(", interfaces: [mcp, agent]{field}")))
                .unwrap_or_else(|e| panic!("{field}: {e}"));
        }
        refused(
            &company(", interfaces: [agent], context: {locale: \"en US\"}"),
            "is not a language tag",
        );
        refused(
            &company(", interfaces: [agent], context: {time_zone: \"/etc/localtime\"}"),
            "is not an IANA time zone name",
        );
        refused(
            &company("").replace("name: Agent}", "name: Agent}\n    context: {locale: en-US}"),
            "security.companies.context: no company entry uses `agent`",
        );
        refused(
            &company(", interfaces: [agent]").replace(
                "name: Agent}",
                "name: Agent}\n    limits: {conversations: 0}",
            ),
            "limits.conversations: 0 would let no conversation open",
        );
        refused(
            &format!(
                "{CLIENT}{STORE}services:\n  m: {{kind: mcp, endpoint: \"https://m.example/mcp\", api_allow: [\"GET /x\"]}}\n"
            ),
            "`api_allow` applies to `kind: company` entries only",
        );
        // An `http` step reaches a Company's API only at an entry that uses
        // `openapi`, and names no credential, body or host rule of its own.
        let flow = |step: &str| {
            format!(
                "{}workflows:\n  - name: w\n    steps:\n      s: {{kind: once}}\n      h: {{kind: http, depends_on: [s], {step}}}\n      f: {{kind: finish, depends_on: [h], status: completed}}\n",
                company(", interfaces: [openapi], api_allow: [\"GET /orders/{order_id}\"]")
            )
        };
        load(&flow(
            "url: \"https://example.com/api/orders/o1\", auth: {kind: company, company: shop}",
        ))
        .unwrap();
        // In the account a label names; a label agentd would never keep is
        // refused at load.
        load(&flow(
            "url: \"https://example.com/api/orders/o1\", auth: {kind: company, company: shop, account: work}",
        ))
        .unwrap();
        refused(
            &flow(
                "url: \"https://example.com/api/orders/o1\", auth: {kind: company, company: shop, account: \"Work Account\"}",
            ),
            "auth.account \"Work Account\" is not an account label",
        );
        refused(
            &flow(
                "url: \"https://example.com/api/orders/o1\", auth: {kind: company, company: other}",
            ),
            "auth.company \"other\" is not a company entry that uses `openapi`",
        );
        refused(
            &flow("url: \"https://example.com/x\", auth: {kind: bearer, company: shop}"),
            "http auth takes {kind: company, company: <entry>, account: <label>}",
        );
        refused(
            &flow(
                "url: \"https://example.com/x\", auth: {kind: company, company: shop}, sign: {secret: x}",
            ),
            "`sign` does not apply to an http step that calls a Company's API",
        );
        assert!(same_template("/orders/{order_id}", "/orders/{id}"));
        assert!(!same_template("/orders/{id}", "/orders/{id}/exchanges"));
        assert!(!same_template("/orders/latest", "/orders/{id}"));
    }

    /// The Operations extension and standing permissions (RFC 0047 §5.3,
    /// §5.8.1): what load takes — the extension listed, the stream the
    /// operations' final states go to, the tools' tags — and each refusal of
    /// a permission that could never apply, named.
    #[cfg(feature = "companies")]
    #[test]
    fn standing_permissions_are_held_to_what_they_can_cover() {
        let with = |client: &str, standing: &str, extra: &str| {
            company(extra).replace(
                "name: Agent}",
                &format!("name: Agent{client}}}\n    standing:\n{standing}"),
            )
        };
        let rule = |fields: &str| {
            format!(
                "      - {{company: shop, terms: [order_id, price], when: \"terms.price < 20\", expires: \"2026-12-31T00:00:00Z\"{fields}}}\n"
            )
        };
        let listed = ", extensions: [operations]";
        let ok = with(
            listed,
            &rule(", via: {tool: \"shop.exchange_*\"}, uses: 3"),
            "",
        );
        if cfg!(feature = "cel") {
            let s = load(&ok).unwrap();
            assert!(lists_operations(&s));
            assert_eq!(s.security.companies.standing[0].uses, Some(3));
            assert!(
                s.streams.contains_key("company.shop"),
                "an entry of MCP servers alone has the stream its operations' final states go to"
            );
            assert_eq!(
                interface_tags(&s).len(),
                2,
                "the operation tools carry its tags"
            );
            let api = with(
                listed,
                &rule(", via: {api: \"POST /orders/{id}/exchanges\"}"),
                ", interfaces: [openapi], api_allow: [\"POST /orders/{order_id}/exchanges\"]",
            );
            load(&api).unwrap();
        } else {
            let e = load(&ok).unwrap_err();
            assert!(
                e.contains("standing[0].when needs the `cel` feature"),
                "{e}"
            );
        }
        let refused = |yaml: &str, why: &str| {
            let e = load(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        refused(
            &with("", &rule(", via: {tool: \"shop.exchange_*\"}"), ""),
            "standing[0]: a standing permission confirms operations, which agentd recognises only once security.companies.client.extensions lists operations",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.exchange_*\"}")
                    .replace("company: shop", "company: other"),
                "",
            ),
            "standing[0].company: \"other\" is not a company entry",
        );
        refused(
            &with(listed, &rule(", via: {}"), ""),
            "standing[0].via: name exactly one channel",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\", api: \"POST /x\"}"),
                "",
            ),
            "standing[0].via: name exactly one channel",
        );
        refused(
            &with(listed, &rule(", via: {tool: \"exchange_*\"}"), ""),
            "standing[0].via.tool: \"exchange_*\" names no tool of company shop's MCP servers — each of its tools is named <server>.<tool> (shop.*)",
        );
        refused(
            &with(
                listed,
                &rule(", via: {api: \"POST /orders/{id}/returns\"}"),
                ", interfaces: [openapi], api_allow: [\"POST /orders/{order_id}/exchanges\"]",
            ),
            "standing[0].via.api: \"POST /orders/{id}/returns\" is not in services.shop.api_allow",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}"),
                ", interfaces: [openapi], api_allow: [\"POST /x\"]",
            ),
            "standing[0].via.tool: company shop uses none of its MCP servers",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}").replace("terms: [order_id, price]", "terms: []"),
                "",
            ),
            "standing[0].terms: list every term key it covers",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}")
                    .replace("terms: [order_id, price]", "terms: [a, a]"),
                "",
            ),
            "standing[0].terms: \"a\" is listed twice",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}").replace("terms: [order_id, price], ", ""),
                "",
            ),
            "missing field `terms`",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}").replace("2026-12-31T00:00:00Z", "next year"),
                "",
            ),
            "standing[0].expires: \"next year\" is not an RFC 3339 time",
        );
        refused(
            &with(listed, &rule(", via: {tool: \"shop.x\"}, uses: 0"), ""),
            "standing[0].uses: 0 would never apply",
        );
        // The accounts it covers, by label: none listed covers `default`.
        refused(
            &with(listed, &rule(", via: {tool: \"shop.x\"}, accounts: []"), ""),
            "standing[0].accounts: an empty list covers no account",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}, accounts: [Work]"),
                "",
            ),
            "standing[0].accounts: \"Work\" is not an account label",
        );
        refused(
            &with(
                listed,
                &rule(", via: {tool: \"shop.x\"}, accounts: [work, work]"),
                "",
            ),
            "standing[0].accounts: \"work\" is listed twice",
        );
        if cfg!(feature = "cel") {
            let s = load(&with(
                listed,
                &rule(", via: {tool: \"shop.x\"}, accounts: [default, work]"),
                "",
            ))
            .unwrap();
            assert_eq!(
                s.security.companies.standing[0].accounts.as_deref(),
                Some(&["default".to_string(), "work".to_string()][..])
            );
            assert_eq!(
                load(&ok).unwrap().security.companies.standing[0].accounts,
                None
            );
        }
        if cfg!(feature = "cel") {
            refused(
                &with(
                    listed,
                    &rule(", via: {tool: \"shop.x\"}").replace("terms.price < 20", "terms.price <"),
                    "",
                ),
                "standing[0].when: CEL parse",
            );
        }
        refused(
            &company("").replace(
                "name: Agent}",
                "name: Agent, extensions: [operations, operations]}",
            ),
            "client.extensions: operations is listed twice",
        );
    }

    /// An operation tool's name is its `api_allow` entry by one fixed rule:
    /// in lower case, each run of other characters one `_`, none at either
    /// end, capped — the operator's words, whatever the Company calls it.
    #[test]
    fn an_operation_tool_is_named_from_its_api_allow_entry() {
        for (op, name) in [
            ("GET /orders/{order_id}", "get_orders_order_id"),
            (
                "POST /orders/{order_id}/exchanges",
                "post_orders_order_id_exchanges",
            ),
            ("GET /", "get"),
            (
                "DELETE /Orders/{ID}//items.json",
                "delete_orders_id_items_json",
            ),
            ("GET /café-menu", "get_caf_menu"),
        ] {
            assert_eq!(operation_name(op), name, "{op}");
        }
        let long = format!("GET /{}/x", "a".repeat(70));
        let capped = operation_name(&long);
        assert_eq!(capped.len(), MAX_OPERATION_NAME);
        assert!(capped.starts_with("get_aaa"), "{capped}");
        let edge = format!("GET /{}/x", "a".repeat(MAX_OPERATION_NAME - 5));
        assert!(
            !operation_name(&edge).ends_with('_'),
            "cut where a `_` falls, it is trimmed: {}",
            operation_name(&edge)
        );
        // A key other than `*` sets the tags of the operation tools it
        // names, with `*`'s; a tool none names carries every tag the entry
        // lists, so a key never takes a tag from a tool it does not name.
        let tags = BTreeMap::from([
            ("*".to_string(), vec!["untrusted_input".to_string()]),
            ("shop.post_*".to_string(), vec!["egress".to_string()]),
            ("shop.get_orders".to_string(), vec!["sensitive".to_string()]),
            (
                "shop.get_orders_*".to_string(),
                vec!["untrusted_input".to_string()],
            ),
        ]);
        use crate::sec::scope::TrifectaTag as T;
        assert_eq!(
            operation_tags(&tags, "shop.post_notes"),
            [T::UntrustedInput, T::Egress]
        );
        assert_eq!(
            operation_tags(&tags, "shop.get_orders_order_id"),
            [T::UntrustedInput]
        );
        assert_eq!(
            operation_tags(&tags, "shop.delete_notes"),
            [T::UntrustedInput, T::Sensitive, T::Egress],
            "named by `*` alone, it carries all the entry's tags"
        );
        let for_mcp = BTreeMap::from([(
            "shop.*".to_string(),
            vec!["untrusted_input".to_string(), "egress".to_string()],
        )]);
        assert_eq!(
            operation_tags(&for_mcp, "shop_api.post_notes"),
            [T::UntrustedInput, T::Egress],
            "a key for the entry's MCP server takes nothing from its operation tools"
        );
    }

    /// A company entry that uses `openapi` and lists operations in
    /// `api_allow` stands for a server they are named under: after the
    /// entry, or as `api_names` names each OpenAPI API — `ns` its own name,
    /// the entry's tags, no MCP ceiling, marked as an OpenAPI API's.
    #[cfg(feature = "companies")]
    #[test]
    fn a_company_entry_names_the_server_of_its_operation_tools() {
        let ops =
            ", interfaces: [openapi], api_allow: [\"GET /orders/{order_id}\", \"POST /notes\"]";
        let s = load(&company(ops)).unwrap();
        let shop = s.mcp.servers.iter().find(|x| x.name == "shop").unwrap();
        assert_eq!(shop.ns.as_deref(), Some("shop"));
        assert_eq!(shop.tags, default_tags());
        assert!(shop.allow.is_none() && shop.exclude.is_empty());
        assert_eq!(
            shop.company.as_ref().map(|c| c.interface),
            Some(Interface::Openapi)
        );
        assert!(is_api_server(&s, "shop") && is_company_server(&s, "shop"));
        assert_eq!(
            api_tool_names(&s, "shop"),
            ["shop.get_orders_order_id", "shop.post_notes"]
        );
        // With an MCP server of its own named after the entry, its operation
        // tools are named under `<entry>_api`, unless api_names names it.
        let both = ", interfaces: [mcp, openapi], api_allow: [\"POST /notes\"]";
        let s = load(&company(both)).unwrap();
        assert!(!is_api_server(&s, "shop") && is_api_server(&s, "shop_api"));
        assert_eq!(api_tool_names(&s, "shop_api"), ["shop_api.post_notes"]);
        let both = ", interfaces: [mcp, openapi], api_allow: [\"POST /notes\"], api_names: {\"https://example.com/openapi.json\": orders}";
        let s = load(&company(both)).unwrap();
        assert!(is_api_server(&s, "orders") && !is_api_server(&s, "shop_api"));
        let both = ", interfaces: [mcp, openapi], api_allow: [\"POST /notes\"], api_names: {\"https://example.com/openapi.json\": shop_api}";
        let s = load(&company(both)).unwrap();
        assert!(!is_api_server(&s, "shop") && is_api_server(&s, "shop_api"));
        assert_eq!(api_tool_names(&s, "shop_api"), ["shop_api.post_notes"]);
        assert_eq!(
            s.mcp
                .servers
                .iter()
                .find(|x| x.name == "shop_api")
                .and_then(|x| x.company.as_ref())
                .and_then(|c| c.url.as_deref()),
            Some("https://example.com/openapi.json")
        );
        // Without operations there is no such server.
        let s = load(&company(", interfaces: [openapi]")).unwrap();
        assert!(company_servers(&s).is_empty());
    }

    /// An operation tool's call is `company.api`'s call of its operation, so
    /// what holds that tool holds it: disabling `company.api` disables it,
    /// narrowing `company.api` narrows it, and the root conversation is
    /// offered it only where `agent.tools` offers `company.api`. A key of the
    /// entry's `tags` written for its MCP server takes no tag from it.
    #[cfg(feature = "companies")]
    #[test]
    fn what_holds_company_api_holds_its_operation_tools() {
        use crate::registry::{Caller, Registry, ServerTools};
        use crate::sec::scope::TrifectaTag as T;
        let entry = company(
            ", interfaces: [mcp, openapi], api_allow: [\"GET /orders/{order_id}\", \"POST /notes\"], api_names: {\"https://example.com/openapi.json\": shop_api}, tags: {\"shop.*\": [untrusted_input, egress], \"shop_api.get_*\": [untrusted_input]}",
        );
        let tool = |n: &str| ::mcp::wire::Tool {
            name: n.into(),
            title: None,
            description: Some(format!("{n}.")),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
        };
        let built = |extra: &str| {
            let s = load(&format!("{entry}{extra}")).unwrap();
            let servers = [ServerTools {
                name: "shop_api".into(),
                ns: Some("shop_api".into()),
                tags: Vec::new(),
                tools: vec![tool("get_orders_order_id"), tool("post_notes")],
            }];
            (Registry::build(&s, &servers).unwrap(), s)
        };
        let (post, get) = ("shop_api.post_notes", "shop_api.get_orders_order_id");
        let (reg, _) = built("");
        assert_eq!(reg.contract_of(post), Some("company.api"));
        assert_eq!(reg.contract_of("company.api"), None);
        assert_eq!(
            reg.tags_of(&[post.to_string()]),
            [T::UntrustedInput, T::Egress],
            "named by no key of its own, it carries all the entry's tags"
        );
        assert_eq!(reg.tags_of(&[get.to_string()]), [T::UntrustedInput]);
        let (reg, _) = built("tools:\n  disabled: [company.api]\n");
        assert!(!reg.allowed(&Caller::Root, post) && !reg.allowed(&Caller::Workflow, get));
        let (reg, _) = built(
            "tools:\n  narrow:\n    company.api: {tags: [sensitive], describe: Ask first.}\n",
        );
        for name in [post, get, "company.api"] {
            let t = reg.get(name).unwrap();
            assert!(t.tags.contains(&T::Sensitive), "{name}: {:?}", t.tags);
            assert!(
                t.description.ends_with("[operator note]: Ask first."),
                "{name}: {}",
                t.description
            );
        }
        let offered = |extra: &str| -> Vec<String> {
            let (reg, s) = built(extra);
            reg.defs_for(&Caller::Root, Some(&s.agent.tools))
                .into_iter()
                .map(|d| d.name)
                .filter(|n| n.starts_with("shop_api.") || n == "company.api")
                .collect()
        };
        assert_eq!(offered(""), ["company.api", get, post]);
        assert!(offered("agent:\n  tools: {internal: [status]}\n").is_empty());
        assert_eq!(
            offered("agent:\n  tools: {internal: [company.api], mcp: [shop_api]}\n"),
            ["company.api", get, post]
        );
    }

    /// What load refuses about operation tools, each named: a server name
    /// taken — by the entry's own MCP server, another entry's, a configured
    /// one — an `api_names` that names nothing or is not one, an operation
    /// listed twice or two named alike, and a tag, a policy, a workflow step
    /// or a standing permission naming an operation tool the entry has not.
    #[cfg(feature = "companies")]
    #[test]
    fn what_load_refuses_about_operation_tools_and_why() {
        let refused = |yaml: &str, why: &str| {
            let e = load(yaml).expect_err(yaml);
            assert!(e.contains(why), "want {why:?} in:\n{e}");
        };
        let ops = |extra: &str| {
            company(&format!(
                ", interfaces: [openapi], api_allow: [\"GET /orders/{{order_id}}\", \"POST /notes\"]{extra}"
            ))
        };
        refused(
            &company(
                ", interfaces: [mcp, openapi], api_allow: [\"POST /notes\"], api_names: {\"https://example.com/openapi.json\": shop}",
            ),
            "services.shop: the server its operation tools are named under \"shop\" has the name of another of its own servers — name them apart with api_names or mcp_names",
        );
        refused(
            &format!(
                "{}mcp:\n  servers:\n    - {{name: shop, endpoint: \"https://other.example/mcp\"}}\n",
                ops("")
            ),
            "has the name of mcp server 'shop' — rename one (api_names names a Company's servers)",
        );
        refused(
            &company(", interfaces: [mcp], api_names: {\"https://example.com/o.json\": o}"),
            "services.shop.api_names: it names the servers the Company's OpenAPI operation tools are named under, and the entry does not use `openapi`",
        );
        refused(
            &company(", interfaces: [openapi], api_names: {\"https://example.com/o.json\": o}"),
            "services.shop.api_names: services.shop.api_allow lists no operation",
        );
        refused(
            &ops(
                ", api_names: {\"http://example.com/o.json\": o, \"https://example.com/p.json\": code}",
            ),
            "is not an HTTPS URL, and an OpenAPI API's url is one",
        );
        refused(
            &ops(", api_names: {\"https://example.com/p.json\": code}"),
            "services.shop.api_names: \"code\" is not a server name",
        );
        refused(
            &ops(
                ", api_names: {\"https://example.com/o.json\": o, \"https://example.com/p.json\": o}",
            ),
            "services.shop.api_names: two OpenAPI APIs are both named \"o\"",
        );
        refused(
            &company(
                ", interfaces: [openapi], api_allow: [\"GET /orders/{order_id}\", \"GET /orders/{id}\"]",
            ),
            "services.shop.api_allow: \"GET /orders/{order_id}\" and \"GET /orders/{id}\" name the same operation",
        );
        refused(
            &company(", interfaces: [openapi], api_allow: [\"GET /a-b\", \"GET /a_b\"]"),
            "services.shop.api_allow: \"GET /a-b\" and \"GET /a_b\" both make the operation tool name \"get_a_b\"",
        );
        refused(
            &ops(", tags: {\"*\": [untrusted_input], \"shop.get_order\": [egress]}"),
            "services.shop.tags: \"shop.get_order\" names no operation tool of \"shop\" — its tools are shop.get_orders_order_id, shop.post_notes",
        );
        load(&ops(
            ", tags: {\"*\": [untrusted_input], \"shop.post_*\": [egress]}",
        ))
        .unwrap();
        refused(
            &ops("").replace(
                STORE,
                &format!(
                    "  policies:\n    - {{match: {{tool: \"shop.delete_*\"}}, action: deny}}\n{STORE}"
                ),
            ),
            "security.policies[0].match.tool: \"shop.delete_*\" names no operation tool of company entry shop",
        );
        let flow = |name: &str| {
            format!(
                "{}workflows:\n  - name: w\n    steps:\n      s: {{kind: once}}\n      t: {{kind: tool, name: {name}, depends_on: [s]}}\n      f: {{kind: finish, depends_on: [t], status: completed}}\n",
                ops("")
            )
        };
        refused(
            &flow("shop.get_order"),
            "workflow \"w\" step \"t\": \"shop.get_order\" names no operation tool of company entry shop",
        );
        load(&flow("shop.get_orders_order_id")).unwrap();
        refused(
            &ops("").replace(
                "name: Agent}",
                "name: Agent, extensions: [operations]}\n    standing:\n      - {company: shop, via: {tool: \"shop.post_*\"}, terms: [a], when: \"true\", expires: \"2026-12-31T00:00:00Z\"}",
            ),
            "standing[0].via.tool: \"shop.post_*\" names operation tools of \"shop\", whose operations are proposed through the Company's API — name the operation with via.api",
        );
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
        assert_eq!(host_of("https://[::1]:8443/x").as_deref(), Some("[::1]"));
        // Only a plain host[:port]: none a browser could read otherwise.
        for url in [
            "https://other.example\\@a.example/x",
            "https://other.example\\a.example",
            "https://user@a.example/x",
            "https://a.example%2f@b.example/",
            "https://a.example:/x",
            "https://a.example:44x/",
            "https://[]/",
            "https://[::1/",
            "https://a example/",
        ] {
            assert_eq!(host_of(url), None, "{url}");
            assert!(!is_https_url(url), "{url}");
        }
        assert_eq!(origin("https://a.example/x/y?z"), Some("https://a.example"));
        assert!(is_https_url("https://a"));
        assert!(!is_https_url("https:///a"));
        assert!(!is_https_url("http://a"));
    }
}
