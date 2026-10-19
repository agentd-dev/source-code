// SPDX-License-Identifier: AGPL-3.0-only
//! agentd's own OAuth client where it has no registration (RFC 0052):
//! `security.client`, published as an OAuth Client ID Metadata Document
//! (draft-ietf-oauth-client-id-metadata-document-02) at the HTTPS URL that is
//! its `client_id`, beside the key set its `private_key_jwt` assertions are
//! checked against.
//!
//! An `auth: {kind: oauth2}` block that names no `client_id` takes this
//! identity ([`Settings::auth_spec`]); a block that names one keeps its own.
//! agentd uses the document only at an authorization server whose metadata
//! advertises `client_id_metadata_document_supported`, which is read where
//! the metadata is — at `--login` and at the first token request — never at
//! load, which dials nothing.
//!
//! Parsed whatever the build; without the `oauth` feature it is refused by
//! name.

use super::companies::{host_of, is_https_url, origin, path_of};
use super::{Auth, AuthKind, ClientAuthMethod, Diagnostics, OAuthGrant, Settings};
use serde::Deserialize;

/// Where the signing key lives when `key_file` names none.
pub const DEFAULT_KEY_FILE: &str = "/var/lib/agentd/client-key";
/// The last path segment of the key set's default URL, beside the document.
pub const DEFAULT_JWKS_NAME: &str = "jwks.json";

/// The device grant's `grant_type` (RFC 8628 §3.4), as a document lists it.
pub const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// `security.client`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Client {
    /// The `client_id`: the HTTPS URL the document is published at.
    pub id: String,
    /// `client_name`, shown on authorization servers' consent pages.
    pub name: String,
    #[serde(default)]
    pub logo_uri: Option<String>,
    /// The key set's URL; unset, `jwks.json` beside the document.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// The ES256 signing key (PKCS#8), created `0600` when absent.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Keys whose public halves are published but never signed with: the
    /// next key before a rotation, the previous one after it.
    #[serde(default)]
    pub published_key_files: Vec<String>,
    /// The loopback URIs the authorization-code grant returns to.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    /// The grants an authorization server may let agentd use the document
    /// for; unset, the device grant alone.
    #[serde(default = "default_grants")]
    pub grants: Vec<OAuthGrant>,
    /// Serve the document and the key set on the A2A listener.
    #[serde(default)]
    pub serve: bool,
}

fn default_grants() -> Vec<OAuthGrant> {
    vec![OAuthGrant::Device]
}

impl Client {
    /// The key set's URL: as configured, or `jwks.json` in the document's
    /// own directory.
    pub fn jwks_uri(&self) -> String {
        match &self.jwks_uri {
            Some(u) => u.clone(),
            None => match self.id.rfind('/') {
                Some(i) => format!("{}{DEFAULT_JWKS_NAME}", &self.id[..=i]),
                None => DEFAULT_JWKS_NAME.to_string(),
            },
        }
    }

    pub fn key_file(&self) -> &str {
        self.key_file.as_deref().unwrap_or(DEFAULT_KEY_FILE)
    }

    /// The `grant_types` the document lists (RFC 7591 §2): each grant's, and
    /// `refresh_token` beside a grant a person signs in with, whose tokens
    /// agentd renews.
    pub fn grant_types(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = self
            .grants
            .iter()
            .map(|g| match g {
                OAuthGrant::Device => DEVICE_CODE,
                OAuthGrant::AuthorizationCode => "authorization_code",
                OAuthGrant::ClientCredentials => "client_credentials",
            })
            .collect();
        if self
            .grants
            .iter()
            .any(|g| *g != OAuthGrant::ClientCredentials)
        {
            out.push("refresh_token");
        }
        out
    }

    /// The `response_types` the document lists: `code` with the
    /// authorization-code grant, and none otherwise (RFC 7591 §2.1).
    pub fn response_types(&self) -> Vec<&'static str> {
        if self.grants.contains(&OAuthGrant::AuthorizationCode) {
            vec!["code"]
        } else {
            Vec::new()
        }
    }
}

/// Why `url` is not a Client Identifier URL (draft §3), or `None` when it is
/// one: `https`, a plain host with no userinfo, a path other than `/` with no
/// `.` or `..` segment, and neither a query nor a fragment. The draft lets a
/// query through with a SHOULD NOT and a `/` path as NOT RECOMMENDED; agentd
/// refuses both, so the document it prints or serves has a path of its own.
pub fn client_id_url_error(url: &str) -> Option<String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Some("is not an HTTPS URL".into());
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.contains('@') {
        return Some("carries userinfo".into());
    }
    if host_of(url).is_none() {
        return Some("has no host every reader of the URL agrees on".into());
    }
    if url.contains('#') {
        return Some("has a fragment".into());
    }
    if url.contains('?') {
        return Some("has a query".into());
    }
    let path = path_of(url);
    if path == "/" {
        return Some("has no path of its own".into());
    }
    let dotted = path.split('/').any(|seg| {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        s == "." || s == ".."
    });
    if dotted {
        return Some("has a `.` or `..` path segment".into());
    }
    None
}

/// A loopback redirect URI (RFC 8252 §7.3): `http://127.0.0.1` or
/// `http://[::1]`, an optional port, and a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loopback {
    /// The address to bind: `127.0.0.1` or `[::1]`.
    pub host: &'static str,
    /// The port the URI names; none lets an authorization server that
    /// allows any port on a loopback address take the one bound.
    pub port: Option<u16>,
    pub path: String,
}

impl Loopback {
    /// The URI for the port actually bound: the configured one as written
    /// when it names a port.
    pub fn uri(&self, bound: u16) -> String {
        format!(
            "http://{}:{}{}",
            self.host,
            self.port.unwrap_or(bound),
            self.path
        )
    }
}

/// The loopback redirect URI `uri` is, or `None`: an IP literal, never the
/// name `localhost` (RFC 8252 §8.3), and no query or fragment.
pub fn loopback(uri: &str) -> Option<Loopback> {
    let rest = uri.strip_prefix("http://")?;
    let slash = rest.find('/')?;
    let (authority, path) = rest.split_at(slash);
    if path.contains(['?', '#']) {
        return None;
    }
    let (host, port) = match authority.strip_prefix("[::1]") {
        Some(after) => ("[::1]", after),
        None => ("127.0.0.1", authority.strip_prefix("127.0.0.1")?),
    };
    let port = match port {
        "" => None,
        p => Some(
            p.strip_prefix(':')
                .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))?
                .parse::<u16>()
                .ok()
                .filter(|n| *n > 0)?,
        ),
    };
    Some(Loopback {
        host,
        port,
        path: path.to_string(),
    })
}

/// The paths the A2A listener answers itself, which no served document may
/// take.
pub fn listener_answers(path: &str) -> bool {
    path == "/" || path.starts_with("/.well-known/") || path.starts_with("/oauth2/")
}

impl Settings {
    /// The runtime spec of an `auth:` block: [`Auth::to_spec`], and for an
    /// `oauth2` block that names no `client_id`, agentd's Client ID Metadata
    /// Document as its client — the document's URL as `client_id`,
    /// authenticated by `private_key_jwt` with `security.client`'s key, read
    /// at each token request as a configured key is. The block's
    /// `client_auth.assertion_aud` still says what the assertion's `aud`
    /// names.
    pub fn auth_spec(&self, auth: &Auth) -> crate::config::AuthSpec {
        let mut spec = auth.to_spec();
        if let (AuthKind::Oauth2, None, Some(c)) =
            (auth.kind, &auth.client_id, &self.security.client)
        {
            spec.client_id = Some(c.id.clone());
            spec.client_auth = Some(crate::config::ClientAuthSpec {
                method: "private_key_jwt".into(),
                key: Some(format!("{{{{secret-file:{}}}}}", c.key_file())),
                kid: None,
                assertion_aud: spec.client_auth.and_then(|a| a.assertion_aud),
            });
            spec.client_id_metadata_document = true;
        }
        spec
    }
}

/// An `oauth2` block that names no `client_id`, and so uses `client`'s
/// document: it reads the authorization server's metadata from its
/// `issuer`, carries no secret, says nothing of the key it signs with, and
/// uses a grant the document lists.
pub(super) fn validate_block(auth: &Auth, client: &Client, ctx: &str) -> Vec<String> {
    let mut out = Vec::new();
    if auth.issuer.is_none() {
        out.push(format!(
            "{ctx}: with no client_id, agentd's Client ID Metadata Document (security.client) is the client, used only where the authorization server's metadata advertises client_id_metadata_document_supported — set `issuer`, whose metadata says so"
        ));
    }
    if auth.client_secret.is_some() {
        out.push(format!(
            "{ctx}: a Client ID Metadata Document client has no client_secret (draft §4.1); remove `client_secret`, or name the registered client's `client_id`"
        ));
    }
    if let Some(c) = &auth.client_auth
        && (c.method != ClientAuthMethod::PrivateKeyJwt || c.key.is_some() || c.kid.is_some())
    {
        out.push(format!(
            "{ctx}: a Client ID Metadata Document client signs with security.client's key, published in its key set; auth.client_auth may only set `assertion_aud`"
        ));
    }
    let grant = auth.grant.unwrap_or(OAuthGrant::Device);
    if !client.grants.contains(&grant) {
        out.push(format!(
            "{ctx}: the {} grant is not in security.client.grants, so the document does not let an authorization server issue agentd tokens by it",
            grant_name(grant)
        ));
    }
    out
}

fn grant_name(g: OAuthGrant) -> &'static str {
    match g {
        OAuthGrant::Device => "device",
        OAuthGrant::AuthorizationCode => "authorization_code",
        OAuthGrant::ClientCredentials => "client_credentials",
    }
}

/// Every check on `security.client` at load.
pub(super) fn validate(s: &Settings, d: &mut Diagnostics) {
    let Some(c) = &s.security.client else {
        return;
    };
    let mut err = |m: String| d.errors.push(m);
    if !cfg!(feature = "oauth") {
        err("security.client needs the `oauth` feature (build with --features oauth)".into());
        return;
    }
    if let Some(why) = client_id_url_error(&c.id) {
        err(format!(
            "security.client.id: {:?} {why}; a client_id is the HTTPS URL of its Client ID Metadata Document (draft §3)",
            c.id
        ));
        return;
    }
    if c.name.trim().is_empty() {
        err("security.client.name: a client_name is required".into());
    }
    if let Some(l) = &c.logo_uri
        && !is_https_url(l)
    {
        err(format!(
            "security.client.logo_uri: {l:?} is not an HTTPS URL"
        ));
    }
    let jwks = c.jwks_uri();
    if !is_https_url(&jwks) {
        err(format!(
            "security.client.jwks_uri: {jwks:?} is not an HTTPS URL"
        ));
    } else if jwks == c.id {
        err(format!(
            "security.client.jwks_uri: {jwks:?} is the client_id; the key set is a document of its own"
        ));
    }
    for (i, r) in c.redirect_uris.iter().enumerate() {
        if loopback(r).is_none() {
            err(format!(
                "security.client.redirect_uris: {r:?} is not a loopback redirect URI — http://127.0.0.1[:port]/path or http://[::1][:port]/path, where agentd's authorization-code login listens (RFC 8252 §7.3)"
            ));
        } else if c.redirect_uris[..i].contains(r) {
            err(format!(
                "security.client.redirect_uris: {r:?} is listed twice"
            ));
        }
    }
    if c.grants.is_empty() {
        err(
            "security.client.grants: lists no grant, so no authorization server would issue agentd a token by the document"
                .into(),
        );
    }
    for (i, g) in c.grants.iter().enumerate() {
        if c.grants[..i].contains(g) {
            err(format!(
                "security.client.grants: {} is listed twice",
                grant_name(*g)
            ));
        }
    }
    match (
        c.grants.contains(&OAuthGrant::AuthorizationCode),
        c.redirect_uris.is_empty(),
    ) {
        (true, true) => err(
            "security.client.grants: the authorization_code grant returns to a redirect URI the document lists; add a loopback one to security.client.redirect_uris".into(),
        ),
        (false, false) => err(
            "security.client.redirect_uris: only the authorization_code grant returns to a redirect URI, and security.client.grants does not list it".into(),
        ),
        _ => {}
    }
    if c.serve {
        validate_serve(s, c, &jwks, &mut err);
    }
}

/// `security.client.serve`: the A2A listener serves both documents at their
/// URLs' paths, on the `client_id`'s origin, beside its own routes and the
/// Companies' served identity.
fn validate_serve(s: &Settings, c: &Client, jwks: &str, err: &mut impl FnMut(String)) {
    if !cfg!(feature = "a2a") {
        err("security.client.serve needs the `a2a` feature: the documents are served on the A2A listener".into());
    }
    match (&s.a2a.listen, &s.a2a.url) {
        (None, _) => err("security.client.serve needs a2a.listen: the documents are served on the A2A listener".into()),
        (_, None) => err("security.client.serve needs a2a.url, the origin authorization servers reach the listener at; it must be the client_id's".into()),
        (_, Some(url)) if origin(url).map(|o| o.trim_end_matches('/')) != origin(&c.id) => err(format!(
            "security.client.serve: a2a.url {url:?} is not the client_id's origin ({}); the listener serves the client_id's own URL",
            origin(&c.id).unwrap_or_default()
        )),
        _ => {}
    }
    if origin(jwks) != origin(&c.id) {
        err(format!(
            "security.client.serve: jwks_uri {jwks:?} is not on the client_id's origin, the only one the listener serves"
        ));
    }
    let mine = [path_of(&c.id), path_of(jwks)];
    for p in &mine {
        if listener_answers(p) {
            err(format!(
                "security.client.serve: {p} is a path the A2A listener answers itself; publish the document and key set elsewhere"
            ));
        }
    }
    let companies = s
        .security
        .companies
        .client
        .as_ref()
        .filter(|cl| cl.serve)
        .map(|cl| super::companies::served_paths(s, cl))
        .unwrap_or_default();
    for p in &mine {
        if companies.contains(p) {
            err(format!(
                "security.client.serve: {p} is also a path of the Companies' served client identity (security.companies.client)"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(yaml: &str) -> Settings {
        Settings::from_document(crate::config::yaml::parse(yaml).unwrap(), "t").unwrap()
    }

    fn errors(yaml: &str) -> Vec<String> {
        let s = settings(yaml);
        let mut d = Diagnostics::default();
        validate(&s, &mut d);
        d.errors
    }

    const CLIENT: &str = "security:\n  client:\n    id: https://agent.example/oauth/client.json\n    name: Dana's agent\n";

    /// The draft's §3 rules: https, a host, a path of its own, no userinfo,
    /// fragment, query or dot segment, compared as written.
    #[test]
    fn a_client_identifier_url_holds_to_the_draft() {
        assert_eq!(
            client_id_url_error("https://agent.example/client.json"),
            None
        );
        assert_eq!(client_id_url_error("https://agent.example:8443/a/b"), None);
        for (url, why) in [
            ("http://agent.example/client.json", "HTTPS"),
            ("https://dana@agent.example/client.json", "userinfo"),
            ("https://agent.example", "no path"),
            ("https://agent.example/", "no path"),
            ("https://agent.example/client.json#x", "fragment"),
            ("https://agent.example/client.json?v=1", "query"),
            ("https://agent.example/a/../client.json", "`..`"),
            ("https://agent.example/./client.json", "`.`"),
            ("https://agent.example/a/%2E%2e/client.json", "`..`"),
            ("https://agent .example/client.json", "host"),
        ] {
            let e = client_id_url_error(url).unwrap_or_else(|| panic!("{url} passed"));
            assert!(e.contains(why), "{url}: {e}");
        }
    }

    /// Only loopback IP literals, with or without a port, and a path.
    #[test]
    fn a_redirect_uri_is_a_loopback_ip_literal() {
        assert_eq!(
            loopback("http://127.0.0.1:8765/callback"),
            Some(Loopback {
                host: "127.0.0.1",
                port: Some(8765),
                path: "/callback".into()
            })
        );
        let any = loopback("http://[::1]/cb").unwrap();
        assert_eq!((any.host, any.port), ("[::1]", None));
        assert_eq!(any.uri(50001), "http://[::1]:50001/cb");
        assert_eq!(
            loopback("http://127.0.0.1:8765/callback").unwrap().uri(1),
            "http://127.0.0.1:8765/callback"
        );
        for bad in [
            "http://localhost:8765/callback",
            "https://127.0.0.1:8765/callback",
            "http://127.0.0.1:8765",
            "http://127.0.0.1:0/cb",
            "http://127.0.0.1:x/cb",
            "http://127.0.0.1:8765/cb?x=1",
            "http://127.0.0.2/cb",
            "http://127.0.0.1.evil.example/cb",
        ] {
            assert!(loopback(bad).is_none(), "{bad}");
        }
    }

    /// The document's grant types and response types follow `grants`, and
    /// the key set sits beside the document unless named.
    #[test]
    fn the_document_lists_the_grants_the_operator_named() {
        let s = settings(CLIENT);
        let c = s.security.client.as_ref().unwrap();
        assert_eq!(
            c.grants,
            [OAuthGrant::Device],
            "the device grant by default"
        );
        assert_eq!(c.grant_types(), [DEVICE_CODE, "refresh_token"]);
        assert!(c.response_types().is_empty());
        assert_eq!(c.jwks_uri(), "https://agent.example/oauth/jwks.json");
        assert_eq!(c.key_file(), DEFAULT_KEY_FILE);
        let s = settings(&format!(
            "{CLIENT}    grants: [client_credentials]\n    jwks_uri: https://keys.example/k.json\n"
        ));
        let c = s.security.client.as_ref().unwrap();
        assert_eq!(c.grant_types(), ["client_credentials"], "nothing to renew");
        assert_eq!(c.jwks_uri(), "https://keys.example/k.json");
        let s = settings(&format!(
            "{CLIENT}    grants: [authorization_code, device]\n    redirect_uris: [\"http://127.0.0.1:8765/callback\"]\n"
        ));
        let c = s.security.client.as_ref().unwrap();
        assert_eq!(
            c.grant_types(),
            ["authorization_code", DEVICE_CODE, "refresh_token"]
        );
        assert_eq!(c.response_types(), ["code"]);
    }

    #[test]
    fn a_valid_client_loads_and_a_broken_one_is_named() {
        assert_eq!(errors(CLIENT), Vec::<String>::new());
        for (extra, want) in [
            ("    logo_uri: http://agent.example/logo.png\n", "logo_uri"),
            (
                "    jwks_uri: https://agent.example/oauth/client.json\n",
                "the key set is a document of its own",
            ),
            (
                "    grants: [authorization_code]\n",
                "add a loopback one to security.client.redirect_uris",
            ),
            (
                "    redirect_uris: [\"http://127.0.0.1:8765/cb\"]\n",
                "does not list it",
            ),
            (
                "    grants: [authorization_code]\n    redirect_uris: [\"http://localhost:8765/cb\"]\n",
                "is not a loopback redirect URI",
            ),
            ("    grants: []\n", "lists no grant"),
            ("    grants: [device, device]\n", "device is listed twice"),
            ("    serve: true\n", "needs a2a.listen"),
        ] {
            let errs = errors(&format!("{CLIENT}{extra}"));
            assert!(
                errs.iter().any(|e| e.contains(want)),
                "{extra:?} → {errs:?} (wanted {want:?})"
            );
        }
        let errs = errors("security:\n  client:\n    id: https://agent.example/\n    name: x\n");
        assert!(errs[0].starts_with("security.client.id"), "{errs:?}");
    }

    /// Served on the A2A listener, on the client_id's origin, at paths of
    /// their own.
    #[test]
    fn a_served_document_is_on_the_listeners_origin_at_its_own_path() {
        let listener = "a2a:\n  listen: 127.0.0.1:0\n  url: https://agent.example\n";
        let served = format!("{CLIENT}    serve: true\n{listener}");
        let errs = errors(&served);
        assert!(
            errs.iter().all(|e| !e.contains("security.client")),
            "{errs:?}"
        );
        let errs = errors(&format!(
            "{CLIENT}    serve: true\na2a:\n  listen: 127.0.0.1:0\n  url: https://other.example\n"
        ));
        assert!(
            errs.iter()
                .any(|e| e.contains("is not the client_id's origin")),
            "{errs:?}"
        );
        let errs = errors(&format!(
            "security:\n  client:\n    id: https://agent.example/.well-known/client.json\n    name: x\n    serve: true\n{listener}"
        ));
        assert!(
            errs.iter().any(|e| e.contains("answers itself")),
            "{errs:?}"
        );
    }

    /// A block with no client_id takes the document: its URL as client_id,
    /// private_key_jwt with the configured key, and the flag that has the
    /// authorization server's advertisement checked. A configured client_id
    /// wins.
    #[test]
    fn a_block_without_a_client_id_takes_the_document() {
        let s = settings(&format!(
            "{CLIENT}    key_file: /keys/agentd.pem\nmcp:\n  servers:\n    - name: m\n      endpoint: https://mcp.example\n      auth: {{kind: oauth2, issuer: \"https://as.example\", client_auth: {{assertion_aud: issuer}}}}\n    - name: n\n      endpoint: https://mcp2.example\n      auth: {{kind: oauth2, issuer: \"https://as.example\", client_id: registered}}\n"
        ));
        let m = s.auth_spec(s.mcp.servers[0].auth.as_ref().unwrap());
        assert_eq!(
            m.client_id.as_deref(),
            Some("https://agent.example/oauth/client.json")
        );
        assert!(m.client_id_metadata_document);
        let ca = m.client_auth.unwrap();
        assert_eq!(ca.method, "private_key_jwt");
        assert_eq!(ca.key.as_deref(), Some("{{secret-file:/keys/agentd.pem}}"));
        assert_eq!(ca.assertion_aud.as_deref(), Some("issuer"));
        let n = s.auth_spec(s.mcp.servers[1].auth.as_ref().unwrap());
        assert_eq!(n.client_id.as_deref(), Some("registered"));
        assert!(!n.client_id_metadata_document);
        assert!(n.client_auth.is_none());
        // Without security.client nothing is filled in.
        let bare = settings(
            "mcp:\n  servers:\n    - name: m\n      endpoint: https://mcp.example\n      auth: {kind: oauth2, issuer: \"https://as.example\"}\n",
        );
        let b = bare.auth_spec(bare.mcp.servers[0].auth.as_ref().unwrap());
        assert!(b.client_id.is_none() && !b.client_id_metadata_document);
    }

    #[test]
    fn a_document_block_names_its_issuer_and_no_secret() {
        let s = settings(CLIENT);
        let c = s.security.client.as_ref().unwrap();
        let block = |yaml: &str| -> Auth {
            serde_json::from_value(crate::config::yaml::parse(yaml).unwrap()).unwrap()
        };
        assert!(
            validate_block(
                &block("{kind: oauth2, issuer: \"https://as.example\"}"),
                c,
                "m"
            )
            .is_empty()
        );
        for (yaml, want) in [
            (
                "{kind: oauth2, token_url: \"https://as.example/t\"}",
                "set `issuer`",
            ),
            (
                "{kind: oauth2, issuer: \"https://as.example\", client_secret: \"{{secret:S}}\"}",
                "has no client_secret",
            ),
            (
                "{kind: oauth2, issuer: \"https://as.example\", client_auth: {key: \"{{secret-file:/k}}\"}}",
                "may only set `assertion_aud`",
            ),
            (
                "{kind: oauth2, issuer: \"https://as.example\", client_auth: {method: client_secret_basic}}",
                "may only set `assertion_aud`",
            ),
            (
                "{kind: oauth2, issuer: \"https://as.example\", grant: client_credentials}",
                "client_credentials grant is not in security.client.grants",
            ),
        ] {
            let errs = validate_block(&block(yaml), c, "m");
            assert!(
                errs.iter().any(|e| e.contains(want)),
                "{yaml} → {errs:?} (wanted {want:?})"
            );
        }
    }
}
