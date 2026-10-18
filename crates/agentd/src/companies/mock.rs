// SPDX-License-Identifier: AGPL-3.0-only
//! The **fake Company**: a Company as the Personal Agent Protocol draft
//! pinned at `95fcbf2` describes one, for agentd's tests and conformance
//! suite to hold a Personal Agent against (rfcs/0047 §9, P0). The draft's
//! repository ships a validator for Companies and nothing that tests a
//! Personal Agent, so agentd writes its own Company. Hidden mode:
//! `agentd --internal-mock-company <addr-file> --cert <pem> --key <pem>
//! [--ca <pem>] [--variant <name>]… [--listen <addr>]`.
//!
//! It binds a loopback TCP listener, serves HTTPS with the identity in
//! `--cert` and `--key`, and writes the bound `host:port` into `<addr-file>`
//! ([`crate::announce_addr`]). It is the Company at `https://127.0.0.1:<port>`:
//! its discovery document names `127.0.0.1` as the organization's domain,
//! its issuer is `https://127.0.0.1:<port>/auth` — with a path, so the RFC
//! 8414 suffix goes before it — and every endpoint it names is on that host.
//! `localhost` reaches the same listener under another name, and is the
//! other host the off-host variants name. It fetches a Personal Agent's
//! client metadata document and key set over HTTPS, trusting the CA in
//! `--ca` as well as the bundled roots.
//!
//! | Request | What it is |
//! |---|---|
//! | `GET` [`wire::DISCOVERY_PATH`] | the discovery document (§3) |
//! | `GET /.well-known/oauth-authorization-server/auth` | RFC 8414 metadata (§3.2) |
//! | `POST /auth/token` | every grant: start and renew a Session, the authorization code, the device code, the Account Token (§4.2–§4.8) |
//! | `POST /auth/device` | device authorization (§4.6) |
//! | `GET /auth/device/verify` | the User's device page, which approves nothing by itself |
//! | `GET /auth/authorize` | Direct Sign-In (§4.5), answered by the scripted User |
//! | `POST /auth/revoke` | RFC 7009 revocation of an Account Token (§4.9) |
//! | `POST /mcp` | an MCP server taking only Bearer tokens issued for its URL (§6); its `search` for `coupon` is the tool's own error, worded with the codes a refusal carries |
//! | `GET /.well-known/oauth-protected-resource/mcp` | its RFC 9728 metadata |
//! | `GET /api/openapi.json`, `GET /api/session` | an OpenAPI description and its one operation, which takes DPoP Session Tokens (§4.3, §6) |
//! | `POST /_mock/control`, `GET /_mock/record` | steer the scripted User and token lifetimes; read what the fake saw |
//!
//! **What it holds a Personal Agent to.** Both assertions are verified
//! against the key set behind the agent's `client_id` (§4.1, §4.2): `iss`,
//! `sub`, `aud` equal to the issuer as one string, a short lifetime, and a
//! `jti` not seen before. Every DPoP Proof is validated as §4.3 lists — one
//! header, `typ`, an accepted `alg`, a public `jwk`, the signature, `htm`
//! and `htu`, the acceptance window, the nonce when one is required, `ath`
//! and the bound key on a request with a Session Token, no `ath` and no
//! replayed `jti` at the token endpoint. A token request's `scope` may ask
//! for fewer scopes than its grant holds, never more. A token request
//! without a proof is for MCP and needs `resource` naming a listed MCP
//! server; the Bearer token it gets works there only, and a DPoP token works
//! everywhere else but there. A refusal is the draft's error, and the fake
//! is strict where the draft is: a Personal Agent tested against it learns
//! of a mistake at the request that makes it.
//!
//! **The scripted User** approves a device or Direct Sign-In as the account
//! `acct_1`, granting every scope asked for, after one `authorization_pending`
//! poll. `POST /_mock/control {"approver": {"decision": "approve|deny|expire",
//! "account": …, "after_polls": n, "scope": "…"}, "expires_in": n}` changes
//! what it does next, and how long Session Tokens last. The same control
//! steers what a Personal Agent is tested against mid-run: `"refuse_tokens":
//! n` answers the next `n` MCP requests `401 invalid_token`;
//! `"refuse_grants": n` answers the next `n` token requests `401
//! invalid_client`;
//! `"extension_required": "<name>"` has the MCP servers refuse every request
//! from then on with `403 extension_required`; `"extra_mcp": true` lists a
//! second MCP API, at `/mcp/orders`, from then on; `"end_sessions": true`
//! ends every Session, so its next renewal is `invalid_session`;
//! `"stall": {"method": m, "times": n, "ms": t}` holds the next `n` MCP
//! requests of method `m` `t` ms before answering them (a Personal Agent
//! whose timeout is shorter sees no answer to a request that arrived);
//! `"fail": {"method": m, "times": n, "status": s}` answers the next `n` of
//! them with the bare status `s` after they arrived — or, with `"error": e`
//! or `"result": r`, with `s` and a JSON-RPC answer carrying that member;
//! `"date_skew_s": n` sets every reply's `Date` `n` seconds from the fake's
//! clock.
//!
//! **Its documents cannot drift from the kit.** Every JSON body it sends is
//! validated against its vendored schema first ([`kit`], patterns and
//! cross-file references included); one that fails is logged
//! `COMPANY_DRIFT <schema> <errors>` and answered `500`. A variant whose flaw
//! breaks a schema applies the flaw after the check.
//!
//! **Its record.** Every request leaves one `COMPANY <json>` line on stderr —
//! method, path, status, the error code, the grant, whether it carried a
//! proof and a `resource`, the tokens it issued; for a request carrying a
//! Session Token, `presented` (the first eight characters of the token's
//! base64url SHA-256) and `superseded` (whether a newer token of its kind
//! had been issued for its Session, which the Personal Agent was to use
//! instead, §4.3); for a token request the claims of each assertion and
//! proof it carried (`client_assertion`, `assertion`, `proof`, the proof's
//! key thumbprint as `jkt`), never their signatures; for every MCP request,
//! the keys of its `_meta` (`meta`), the trace id its `traceparent` names
//! (`trace_id`), the `clientInfo.title` it carries (`client_title`), the
//! `Mcp-Session-Id` it carries (`mcp_session`), and for a `tools/call` its
//! `agent/idempotency_key` (`idempotency_key`); for any
//! request, whether it carried an AAuth signature (`aauth`), a
//! `traceparent` header (`trace_header`) or a `User-Agent` (`user_agent`);
//! and `validated`, the vendored schemas what the request carried held to —
//! and the same objects are read back at `GET /_mock/record`: the server
//! side's own account of what reached it, which a test counts instead of
//! trusting the client.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
use serde_json::{Map, Value, json};

use super::wire::{self, error as err, grant};
use crate::auth::client_assertion::ASSERTION_TYPE;
use crate::auth::jwk;
use crate::b64;
use crate::net::tls::{ServerIdentity, TlsAcceptor};

pub mod kit;

/// A deliberate departure from a conforming Company, or a shape a Personal
/// Agent must handle: what the tests and the conformance checks hold a
/// Personal Agent's behaviour against (rfcs/0047 §9, P0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// The metadata's `issuer` is the document's `auth.issuer` with a
    /// trailing `/`: equal to a lenient reader, not byte for byte (§3.2).
    IssuerMismatch,
    /// The metadata lists no domains for the issuer (§3.2).
    NoDomains,
    /// The DPoP algorithms leave out `ES256` (§3.2).
    NoEs256,
    /// The DPoP algorithms are published only under the prefixed member
    /// issue #12 reports.
    PrefixedAlgs,
    /// The metadata's endpoints are on another host.
    OffHostEndpoints,
    /// The device authorization sends the User to another host.
    OffHostVerificationUri,
    /// The discovery document redirects to plain HTTP (§3).
    RedirectToHttp,
    /// Every proof must carry a nonce the fake issued (§4.3).
    NonceRequired,
    /// Each use of an Account Token returns a new one, and the old one is
    /// spent (§4.8).
    Rotation,
    /// The first token request, and the first MCP `tools/call`, are answered
    /// `429` with `Retry-After`, as is anything sent there before the wait
    /// is over (§4.3).
    RetryAfter,
    /// The MCP server speaks only MCP 2025-06-18, the oldest revision a
    /// listed one may (§6).
    Mcp20250618,
    /// Two MCP servers are listed, at `/mcp` and `/mcp/orders`.
    TwoMcpApis,
}

impl Variant {
    pub const ALL: [Variant; 12] = [
        Variant::IssuerMismatch,
        Variant::NoDomains,
        Variant::NoEs256,
        Variant::PrefixedAlgs,
        Variant::OffHostEndpoints,
        Variant::OffHostVerificationUri,
        Variant::RedirectToHttp,
        Variant::NonceRequired,
        Variant::Rotation,
        Variant::RetryAfter,
        Variant::Mcp20250618,
        Variant::TwoMcpApis,
    ];

    /// The name `--variant` takes.
    pub fn name(self) -> &'static str {
        match self {
            Variant::IssuerMismatch => "issuer-mismatch",
            Variant::NoDomains => "no-domains",
            Variant::NoEs256 => "no-es256",
            Variant::PrefixedAlgs => "prefixed-algs",
            Variant::OffHostEndpoints => "off-host-endpoints",
            Variant::OffHostVerificationUri => "off-host-verification-uri",
            Variant::RedirectToHttp => "redirect-to-http",
            Variant::NonceRequired => "nonce-required",
            Variant::Rotation => "rotation",
            Variant::RetryAfter => "retry-after",
            Variant::Mcp20250618 => "mcp-2025-06-18",
            Variant::TwoMcpApis => "two-mcp-apis",
        }
    }

    pub fn parse(name: &str) -> Option<Variant> {
        Variant::ALL.into_iter().find(|v| v.name() == name)
    }
}

/// What the fake serves and how ([`run_with`]).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The server certificate chain and key, PEM files.
    pub cert: String,
    pub key: String,
    /// A CA PEM file trusted, beside the bundled roots, when the fake fetches
    /// a Personal Agent's documents.
    pub ca: Option<String>,
    pub variants: Vec<Variant>,
    /// Where it listens (`127.0.0.1:0`, a free port, when unset).
    pub listen: Option<String>,
}

/// The organization's domain: the host the fake serves its discovery
/// document on.
const DOMAIN: &str = "127.0.0.1";
/// How long a proof or an assertion stays acceptable, either side of now.
const WINDOW_S: u64 = 60;
/// The longest assertion lifetime the fake accepts: the draft asks for
/// short-lived ones (§4.2) and gives a minute as the example.
const MAX_ASSERTION_S: u64 = 300;
/// How long a nonce the fake issued stays good.
const NONCE_S: u64 = 300;
/// The wait a `RetryAfter` refusal asks for, in seconds.
const RETRY_AFTER_S: u64 = 2;
/// The device poll interval it asks for. The draft's example has 5 s; a
/// second keeps a test quick.
const DEVICE_INTERVAL_S: u64 = 1;
const DEVICE_EXPIRES_S: u64 = 600;
/// The scopes each sign-in type can grant (§3.1, §4.4).
const SIGN_IN_SCOPES: [&str; 2] = [wire::SCOPE_READ, wire::SCOPE_WRITE];
/// The private JWK members a key set or a proof must never carry.
const PRIVATE_MEMBERS: [&str; 7] = ["d", "p", "q", "dp", "dq", "qi", "k"];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// What the request this thread is handling showed: its assertions'
    /// and proof's claims, and the schemas it was held to. Each request is
    /// handled on a thread of its own; [`Company::handle`] empties this
    /// into the request's record.
    static SEEN: RefCell<Map<String, Value>> = RefCell::new(Map::new());
}

/// Note `value` under `key` in this request's record.
fn seen(key: &str, value: Value) {
    SEEN.with(|s| {
        s.borrow_mut().insert(key.to_string(), value);
    });
}

/// Note that what the request carried held to the vendored `schema`.
fn validated(schema: &str) {
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let list = s.entry("validated").or_insert_with(|| json!([]));
        if let Some(a) = list.as_array_mut()
            && !a.iter().any(|x| x == schema)
        {
            a.push(json!(schema));
        }
    });
}

fn now() -> u64 {
    crate::auth::cache::now_ms() / 1000
}

/// A fresh opaque value: `prefix` and 16 random bytes, base64url.
fn opaque(prefix: &str) -> String {
    // The kernel CSPRNG has no fallback; a fake that cannot draw one has
    // nothing safe to issue, and says so in every token it would have.
    let r = crate::sec::random::b64url_token(16).unwrap_or_else(|_| "no-randomness".into());
    format!("{prefix}{r}")
}

/// Serve the fake on loopback until the process is killed, announcing the
/// bound address through `addr_file`. Returns the process exit code.
pub fn run_with(addr_file: &str, opts: &Options) -> i32 {
    let read = |what: &str, path: &str| {
        std::fs::read(path).map_err(|e| eprintln!("internal-mock-company: {what} {path}: {e}"))
    };
    let (Ok(cert), Ok(key)) = (read("--cert", &opts.cert), read("--key", &opts.key)) else {
        return 2;
    };
    let ca = match opts.ca.as_deref().map(|p| read("--ca", p)) {
        Some(Err(())) => return 2,
        Some(Ok(pem)) => Some(pem),
        None => None,
    };
    let acceptor =
        match ServerIdentity::from_pem(&cert, &key).and_then(|id| TlsAcceptor::new(id, None)) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                eprintln!("internal-mock-company: server identity: {e}");
                return 2;
            }
        };
    let at = opts.listen.as_deref().unwrap_or("127.0.0.1:0");
    let listener = match TcpListener::bind(at) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("internal-mock-company: bind {at}: {e}");
            return 1;
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("internal-mock-company: local address: {e}");
            return 1;
        }
    };
    if let Err(e) = crate::announce_addr(addr_file, &listener) {
        eprintln!("internal-mock-company: write {addr_file}: {e}");
        return 1;
    }
    let company = Arc::new(Company::new(port, opts.variants.clone(), ca));
    for conn in listener.incoming() {
        let Ok(tcp) = conn else { continue };
        let (company, acceptor) = (Arc::clone(&company), Arc::clone(&acceptor));
        std::thread::spawn(move || {
            tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
            if let Ok(mut tls) = acceptor.accept(tcp)
                && let Some(req) = read_request(&mut tls)
            {
                let reply = company.handle(&req);
                write_reply(&mut tls, &reply);
            }
        });
    }
    0
}

/// One HTTP request as the fake read it.
struct Request {
    method: String,
    /// The path, without the query.
    path: String,
    query: String,
    /// Every header, its name lower-cased, duplicates kept in arrival order.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The URL the request was sent to, without query or fragment, as a
    /// proof's `htu` names it.
    fn htu(&self) -> String {
        format!(
            "https://{}{}",
            self.header("host").unwrap_or_default(),
            self.path
        )
    }
}

fn read_request(stream: &mut impl Read) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0usize)
        .min(1 << 20);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

/// One answer, and what the fake notes about it in its record.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    note: Map<String, Value>,
}

impl Reply {
    fn new(status: u16) -> Reply {
        Reply {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            note: Map::new(),
        }
    }

    fn json(status: u16, body: &Value) -> Reply {
        let mut r = Reply::new(status).with("Content-Type", "application/json");
        r.body = serde_json::to_vec(body).unwrap_or_default();
        r
    }

    fn with(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn note(mut self, key: &str, value: Value) -> Reply {
        self.note.insert(key.to_string(), value);
        self
    }
}

fn write_reply(stream: &mut impl Write, r: &Reply) {
    let reason = match r.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", r.status);
    for (k, v) in &r.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        r.body.len()
    ));
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&r.body);
    let _ = stream.flush();
}

/// A form or query string, decoded. A parameter given twice is refused, as
/// RFC 6749 §3.1 says, and one given empty is absent.
fn decode_form(raw: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for pair in raw.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.iter().position(|b| *b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &[][..]),
        };
        let (k, v) = (pct_decode(k)?, pct_decode(v)?);
        if !seen.insert(k.clone()) {
            return Err(format!("the parameter {k:?} is given more than once"));
        }
        if !v.is_empty() {
            out.insert(k, v);
        }
    }
    Ok(out)
}

fn pct_decode(raw: &[u8]) -> Result<String, String> {
    let bad = || "malformed percent-encoding".to_string();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = raw.get(i + 1..i + 3).ok_or_else(bad)?;
                let hex = std::str::from_utf8(hex).map_err(|_| bad())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| "not UTF-8".to_string())
}

/// A query string from `pairs`, percent-encoded.
fn encode_query(pairs: &[(&str, &str)]) -> String {
    let mut form = crate::auth::oauth2::Form::new();
    for (k, v) in pairs {
        form.field(k, v);
    }
    String::from_utf8(form.finish()).unwrap_or_default()
}

/// A compact JWS, decoded but not verified.
struct Jws {
    header: Value,
    claims: Value,
    input: String,
    sig: Vec<u8>,
}

fn decode_jws(token: &str) -> Result<Jws, String> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a compact JWS (three dot-separated parts)".into());
    };
    let json = |seg: &str, what: &str| -> Result<Value, String> {
        let bytes = b64::url_decode(seg).map_err(|e| format!("{what}: {e}"))?;
        match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => Ok(v),
            _ => Err(format!("{what} is not a JSON object")),
        }
    };
    Ok(Jws {
        header: json(h, "the header")?,
        claims: json(c, "the payload")?,
        input: format!("{h}.{c}"),
        sig: b64::url_decode(s).map_err(|e| format!("the signature: {e}"))?,
    })
}

/// The uncompressed point of a P-256 public JWK.
fn p256_point(key: &Value) -> Option<Vec<u8>> {
    if key["kty"] != "EC" || key["crv"] != "P-256" {
        return None;
    }
    let x = b64::url_decode(key["x"].as_str()?).ok()?;
    let y = b64::url_decode(key["y"].as_str()?).ok()?;
    if x.len() != 32 || y.len() != 32 {
        return None;
    }
    let mut point = vec![0x04];
    point.extend(x);
    point.extend(y);
    Some(point)
}

/// Whether `jws` is signed ES256 by the P-256 public JWK `key`.
fn verify_es256(key: &Value, jws: &Jws) -> bool {
    p256_point(key).is_some_and(|point| {
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(jws.input.as_bytes(), &jws.sig)
            .is_ok()
    })
}

/// A refusal at an OAuth endpoint: its status and OAuth error.
struct Refusal {
    status: u16,
    code: &'static str,
    why: String,
    /// The nonce a `use_dpop_nonce` refusal hands out.
    nonce: Option<String>,
}

fn refuse(status: u16, code: &'static str, why: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        why: why.into(),
        nonce: None,
    }
}

/// What a request's DPoP Proof established.
struct Proof {
    /// The RFC 7638 thumbprint of its key: what a token is bound to.
    jkt: String,
}

/// Why a proof was refused.
enum ProofError {
    Invalid(String),
    /// A nonce is required, and this one to use.
    Nonce(String),
}

/// What a Session Token or a Bearer token is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// A DPoP key, by thumbprint.
    Dpop(String),
    /// An MCP server, by URL.
    Bearer(String),
}

/// What a grant gives the token the request gets: its Session, the scopes
/// it carries, and the Account Token the response hands over — the one a
/// sign-in issues, or the one a rotation replaced the used one with.
struct Granted {
    session_id: String,
    scope: Vec<String>,
    account_token: Option<String>,
}

struct Client {
    keys: Vec<Value>,
    redirect_uris: Vec<String>,
}

struct Session {
    client_id: String,
    user_id: String,
    signed_in: bool,
    /// The account the Session was ever signed in to: it can never switch
    /// to another (§4.4).
    account: Option<String>,
    scope: String,
    /// The sign-in it is signed in with: the [`Account::grant`] of its
    /// Account Token, whichever rotation of it the Session last used.
    grant: Option<String>,
}

struct Access {
    session_id: String,
    binding: Binding,
    signed_in: bool,
    scope: String,
    expires_at: u64,
    /// Its place in the order the fake issued tokens, which says whether a
    /// newer one replaced it ([`Company::superseded`]).
    seq: u64,
}

#[derive(Clone)]
struct Account {
    client_id: String,
    user_id: String,
    account: String,
    scope: Vec<String>,
    /// The sign-in that issued it, which every Account Token rotated from
    /// it shares: revoking the current one revokes the sign-in, and signs
    /// out every Session signed in with any of them (§4.9).
    grant: String,
    revoked: bool,
    /// Replaced by a rotation ([`Variant::Rotation`]): it gets nothing, and
    /// revoking it revokes nothing, since the Personal Agent was to use the
    /// new one from then on (§4.8).
    spent: bool,
}

struct Device {
    client_id: String,
    scope: Vec<String>,
    expires_at: u64,
    interval: u64,
    polls: u32,
    last_poll: Option<Instant>,
    done: bool,
}

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    scope: Vec<String>,
    account: String,
    used: bool,
    expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Approve,
    Deny,
    Expire,
}

/// The scripted User, who answers every sign-in.
struct Approver {
    decision: Decision,
    account: String,
    after_polls: u32,
    /// The scopes approved, when fewer than asked for.
    scope: Option<Vec<String>>,
}

#[derive(Default)]
struct Limit {
    tripped: bool,
    until: Option<Instant>,
}

/// What the next MCP requests of one method meet (`stall`, `fail`).
struct Hold {
    method: String,
    times: u32,
    /// Hold it this long before answering.
    ms: u64,
    /// Answer it with this status instead: bare, or with a JSON-RPC body
    /// whose `error` or `result` is `body`'s.
    status: Option<u16>,
    body: Option<Value>,
}

struct State {
    client_jtis: BTreeMap<String, u64>,
    assertion_jtis: BTreeMap<String, u64>,
    proof_jtis: BTreeMap<String, u64>,
    nonces: BTreeMap<String, u64>,
    sessions: BTreeMap<String, Session>,
    access: BTreeMap<String, Access>,
    accounts: BTreeMap<String, Account>,
    devices: BTreeMap<String, Device>,
    codes: BTreeMap<String, Code>,
    approver: Approver,
    expires_in: u64,
    limits: BTreeMap<&'static str, Limit>,
    records: Vec<Value>,
    /// How many tokens the fake has issued: the next [`Access::seq`].
    issued: u64,
    /// MCP requests still to answer `401 invalid_token`, whatever they carry.
    refuse_tokens: u32,
    /// Token requests still to answer `401 invalid_client`.
    refuse_grants: u32,
    /// The extension the MCP servers refuse every request without.
    extension_required: Option<String>,
    /// MCP requests to hold or fail, in the order the control set them.
    holds: Vec<Hold>,
    /// How far every reply's `Date` is from the fake's clock.
    date_skew_s: i64,
}

/// The fake Company.
struct Company {
    port: u16,
    variants: Vec<Variant>,
    ca: Option<Vec<u8>>,
    /// The Personal Agents whose documents it has fetched, by `client_id`.
    clients: Mutex<BTreeMap<String, Arc<Client>>>,
    state: Mutex<State>,
    /// A second MCP API listed from now on (`"extra_mcp"` in the control).
    extra_mcp: AtomicBool,
}

impl Company {
    fn new(port: u16, variants: Vec<Variant>, ca: Option<Vec<u8>>) -> Company {
        Company {
            port,
            variants,
            ca,
            clients: Mutex::new(BTreeMap::new()),
            state: Mutex::new(State {
                client_jtis: BTreeMap::new(),
                assertion_jtis: BTreeMap::new(),
                proof_jtis: BTreeMap::new(),
                nonces: BTreeMap::new(),
                sessions: BTreeMap::new(),
                access: BTreeMap::new(),
                accounts: BTreeMap::new(),
                devices: BTreeMap::new(),
                codes: BTreeMap::new(),
                approver: Approver {
                    decision: Decision::Approve,
                    account: "acct_1".into(),
                    after_polls: 1,
                    scope: None,
                },
                expires_in: 3600,
                limits: BTreeMap::new(),
                records: Vec::new(),
                issued: 0,
                refuse_tokens: 0,
                refuse_grants: 0,
                extension_required: None,
                holds: Vec::new(),
                date_skew_s: 0,
            }),
            extra_mcp: AtomicBool::new(false),
        }
    }

    fn has(&self, v: Variant) -> bool {
        self.variants.contains(&v)
    }

    fn origin(&self) -> String {
        format!("https://127.0.0.1:{}", self.port)
    }

    fn issuer(&self) -> String {
        format!("{}/auth", self.origin())
    }

    /// Where the metadata's endpoints are: under the issuer, or, with
    /// [`Variant::OffHostEndpoints`], the same paths on the other host.
    fn endpoints(&self) -> String {
        if self.has(Variant::OffHostEndpoints) {
            format!("https://localhost:{}/auth", self.port)
        } else {
            self.issuer()
        }
    }

    fn mcp_urls(&self) -> Vec<String> {
        let mut urls = vec![format!("{}/mcp", self.origin())];
        if self.has(Variant::TwoMcpApis) || self.extra_mcp.load(Ordering::SeqCst) {
            urls.push(format!("{}/mcp/orders", self.origin()));
        }
        urls
    }

    fn api_url(&self) -> String {
        format!("{}/api/openapi.json", self.origin())
    }

    fn handle(&self, req: &Request) -> Reply {
        SEEN.with(|s| s.borrow_mut().clear());
        let skew = lock(&self.state).date_skew_s;
        let reply = self.route(req).with(
            "Date",
            &imf_date(
                i64::try_from(now())
                    .unwrap_or(i64::MAX)
                    .saturating_add(skew),
            ),
        );
        let mut record = json!({
            "method": req.method,
            "path": req.path,
            "status": reply.status,
        });
        for (k, v) in SEEN.with(|s| std::mem::take(&mut *s.borrow_mut())) {
            record[k] = v;
        }
        for (k, v) in &reply.note {
            record[k] = v.clone();
        }
        // What a request says about its sender beside its credential.
        if req.headers.iter().any(|(k, _)| {
            matches!(
                k.as_str(),
                "signature" | "signature-input" | "signature-key"
            ) || k.starts_with("aauth")
        }) {
            record["aauth"] = json!(true);
        }
        if req.header("traceparent").is_some() {
            record["trace_header"] = json!(true);
        }
        if let Some(ua) = req.header("user-agent") {
            record["user_agent"] = json!(ua);
        }
        let mut st = lock(&self.state);
        // A Session Token comes only in `Authorization`, at the API or MCP.
        if let Some((_, token)) = req.header("authorization").and_then(|a| a.split_once(' ')) {
            record["presented"] = json!(presented(token));
            if let Some(a) = st.access.get(token) {
                record["superseded"] = json!(Company::superseded(a, &st));
            }
        }
        eprintln!("COMPANY {record}");
        st.records.push(record);
        reply
    }

    /// Whether a newer token of the same kind — DPoP, or Bearer for the same
    /// MCP server — was issued for `a`'s Session: the Personal Agent was to
    /// use that one from then on (§4.3). The fake still takes `a` until it
    /// expires, as a Company may; the record says it was superseded.
    fn superseded(a: &Access, st: &State) -> bool {
        st.access.values().any(|b| {
            b.session_id == a.session_id
                && b.seq > a.seq
                && match (&a.binding, &b.binding) {
                    (Binding::Dpop(_), Binding::Dpop(_)) => true,
                    (old, new) => old == new,
                }
        })
    }

    fn route(&self, req: &Request) -> Reply {
        let rms = format!("/.well-known/{}", wire::RESOURCE_METADATA_SUFFIX);
        let asm = format!("/.well-known/{}/auth", wire::AS_METADATA_SUFFIX);
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", p) if p == wire::DISCOVERY_PATH => self.discovery(),
            ("GET", p) if p == asm => self.metadata(),
            ("POST", "/auth/token") => self.token(req),
            ("POST", "/auth/device") => self.device_authorization(req),
            ("GET", "/auth/device/verify") => {
                let mut r = Reply::new(200).with("Content-Type", "text/html; charset=utf-8");
                r.body = b"<!doctype html><title>Sign in</title><p>Enter the code your agent shows you.</p>".to_vec();
                r
            }
            ("GET", "/auth/authorize") => self.authorize(req),
            ("POST", "/auth/revoke") => self.revoke(req),
            ("GET", p) if p.strip_prefix(rms.as_str()).is_some_and(|r| self.is_mcp(r)) => {
                self.resource_metadata(&p[rms.len()..])
            }
            (_, p) if self.is_mcp(p) => self.mcp(req),
            ("GET", "/api/openapi.json") => self.openapi(),
            ("GET", "/api/session") => self.session_operation(req),
            ("POST", "/_mock/control") => self.control(req),
            ("GET", "/_mock/record") => {
                Reply::json(200, &json!({"records": lock(&self.state).records}))
            }
            _ => Reply::new(404),
        }
    }

    fn is_mcp(&self, path: &str) -> bool {
        let origin = self.origin();
        self.mcp_urls().iter().any(|u| u[origin.len()..] == *path)
    }

    /// `body`, if it holds to the vendored `schema`; otherwise the `500` that
    /// says the fake has drifted from the kit.
    fn conformant(&self, schema: &str, body: Value) -> Result<Value, Reply> {
        match kit::kit().validate(schema, &body) {
            Ok(()) => Ok(body),
            Err(e) => {
                let errors = e.join("; ");
                eprintln!("COMPANY_DRIFT {schema} {errors}");
                Err(Reply::json(
                    500,
                    &json!({"error": "server_error", "error_description": format!("the fake Company's own document breaks {schema}: {errors}")}),
                )
                .note("drift", json!(schema)))
            }
        }
    }

    /// An OAuth error response (RFC 6749 §5.2), held to the kit's schema.
    fn oauth_error(&self, r: Refusal) -> Reply {
        let body = json!({"error": r.code, "error_description": r.why});
        let mut reply = match self.conformant("oauth-error.schema.json", body) {
            Ok(b) => Reply::json(r.status, &b),
            Err(drift) => return drift,
        };
        reply = reply
            .with("Cache-Control", "no-store")
            .note("error", json!(r.code));
        if r.code == err::RATE_LIMITED {
            reply = reply.with("Retry-After", &RETRY_AFTER_S.to_string());
        }
        if let Some(n) = r.nonce {
            reply = reply.with(wire::DPOP_NONCE_HEADER, &n);
        }
        reply
    }

    // --- Discovery (§3, §3.2) ------------------------------------------

    fn discovery(&self) -> Reply {
        if self.has(Variant::RedirectToHttp) {
            let to = format!("http://127.0.0.1:{}{}", self.port, wire::DISCOVERY_PATH);
            return Reply::new(301).with("Location", &to);
        }
        let mut apis: Vec<Value> = self
            .mcp_urls()
            .into_iter()
            .map(|url| json!({"type": wire::API_MCP, "url": url, "description": "Product search and order status"}))
            .collect();
        apis.push(json!({"type": wire::API_OPENAPI, "url": self.api_url(), "description": "The Session a token belongs to"}));
        let doc = json!({
            "protocol_version": wire::PROTOCOL_VERSION,
            "organization": {"name": "Fake Company", "domain": DOMAIN},
            "auth": {
                "issuer": self.issuer(),
                "direct": {"scopes": SIGN_IN_SCOPES},
                "device": {"scopes": SIGN_IN_SCOPES}
            },
            "apis": apis
        });
        match self.conformant("poppy.schema.json", doc) {
            Ok(doc) => Reply::json(200, &doc).with("Cache-Control", "max-age=300"),
            Err(drift) => drift,
        }
    }

    fn metadata(&self) -> Reply {
        let at = self.endpoints();
        let mut doc = json!({
            "issuer": self.issuer(),
            "token_endpoint": format!("{at}/token"),
            "revocation_endpoint": format!("{at}/revoke"),
            "authorization_endpoint": format!("{at}/authorize"),
            "device_authorization_endpoint": format!("{at}/device"),
            "grant_types_supported": [grant::JWT_BEARER, grant::AUTHORIZATION_CODE, grant::DEVICE_CODE, grant::REFRESH_TOKEN],
            "token_endpoint_auth_methods_supported": ["private_key_jwt"],
            "token_endpoint_auth_signing_alg_values_supported": [wire::ES256],
            "code_challenge_methods_supported": ["S256"],
            "authorization_response_iss_parameter_supported": true,
            "scopes_supported": SIGN_IN_SCOPES,
        });
        doc[wire::DPOP_ALGS_MEMBER] = json!([wire::ES256]);
        doc[wire::DOMAINS_MEMBER] = json!([DOMAIN]);
        let mut doc = match self.conformant("authorization-server-metadata.schema.json", doc) {
            Ok(doc) => doc,
            Err(drift) => return drift,
        };
        // The flaws, after the check: some break the schema on purpose.
        if self.has(Variant::IssuerMismatch) {
            doc["issuer"] = json!(format!("{}/", self.issuer()));
        }
        if self.has(Variant::NoDomains)
            && let Some(m) = doc.as_object_mut()
        {
            m.remove(wire::DOMAINS_MEMBER);
        }
        if self.has(Variant::NoEs256) {
            doc[wire::DPOP_ALGS_MEMBER] = json!(["RS256"]);
        }
        if self.has(Variant::PrefixedAlgs)
            && let Some(m) = doc.as_object_mut()
            && let Some(algs) = m.remove(wire::DPOP_ALGS_MEMBER)
        {
            m.insert(wire::PREFIXED_DPOP_ALGS_MEMBER.to_string(), algs);
        }
        Reply::json(200, &doc).with("Cache-Control", "max-age=300")
    }

    fn resource_metadata(&self, path: &str) -> Reply {
        let doc = json!({
            "resource": format!("{}{path}", self.origin()),
            "authorization_servers": [self.issuer()],
            "bearer_methods_supported": ["header"],
            "scopes_supported": SIGN_IN_SCOPES,
        });
        match self.conformant("protected-resource-metadata.schema.json", doc) {
            Ok(doc) => Reply::json(200, &doc),
            Err(drift) => drift,
        }
    }

    fn openapi(&self) -> Reply {
        Reply::json(
            200,
            &json!({
                "openapi": "3.1.0",
                "info": {"title": "Fake Company", "version": "1"},
                "servers": [{"url": format!("{}/api", self.origin())}],
                "paths": {"/session": {"get": {
                    "operationId": "getSession",
                    "summary": "The Session the presented token belongs to",
                    "responses": {"200": {"description": "The Session", "content": {"application/json": {"schema": {
                        "type": "object",
                        "properties": {"session_id": {"type": "string"}, "signed_in": {"type": "boolean"}, "scope": {"type": "string"}}
                    }}}}}
                }}},
                "components": {"securitySchemes": {"dpop": {"type": "http", "scheme": wire::TOKEN_TYPE_DPOP}}},
                "security": [{"dpop": []}]
            }),
        )
    }

    // --- Agent identity (§4.1) -----------------------------------------

    /// GET a Personal Agent's document over HTTPS, following no redirect.
    fn fetch(&self, url: &str) -> Result<Value, String> {
        use crate::net::http::{self, Url};
        let u = Url::parse(url)?;
        if !u.is_tls() {
            return Err(format!("{url} is not an HTTPS URL"));
        }
        let tcp = http::connect_tcp(&u.host, u.port, Duration::from_secs(5))
            .map_err(|e| format!("connect {url}: {e}"))?;
        let mut tls = match &self.ca {
            Some(ca) => crate::net::tls::connect_with_ca(tcp, &u.host, ca, None),
            None => crate::net::tls::connect(tcp, &u.host, None),
        }
        .map_err(|e| format!("tls {url}: {e}"))?;
        let resp = http::send(
            &mut tls,
            &u.host_header(),
            "GET",
            &u.path,
            &[("Accept", "application/json")],
            &[],
        )
        .map_err(|e| format!("GET {url}: {e}"))?;
        if resp.status != 200 {
            return Err(format!("GET {url}: HTTP {}", resp.status));
        }
        serde_json::from_slice(&resp.body).map_err(|e| format!("GET {url}: not JSON: {e}"))
    }

    /// The Personal Agent `client_id` names: its metadata document checked
    /// as §4.1 says and its key set fetched, cached — and fetched again
    /// when an assertion names a key the cached set does not hold.
    fn client(&self, client_id: &str, kid: Option<&str>) -> Result<Arc<Client>, String> {
        if let Some(c) = lock(&self.clients).get(client_id)
            && kid.is_none_or(|k| c.keys.iter().any(|j| j["kid"] == k))
        {
            return Ok(Arc::clone(c));
        }
        if !(kit::check_for("^https://[^\\s/?#]+").is_some_and(|p| (p.check)(client_id))) {
            return Err("the client_id is not an HTTPS URL".into());
        }
        let doc = self.fetch(client_id)?;
        kit::kit()
            .validate("client-metadata.schema.json", &doc)
            .map_err(|e| format!("the client metadata document: {}", e.join("; ")))?;
        validated("client-metadata.schema.json");
        if doc["client_id"] != client_id {
            return Err(
                "the client metadata document's client_id is not the URL it was fetched from"
                    .into(),
            );
        }
        let host = |u: &str| crate::net::http::Url::parse(u).map(|u| u.host).ok();
        let home = host(client_id);
        let jwks_uri = doc["jwks_uri"].as_str().unwrap_or_default();
        if host(jwks_uri) != home {
            return Err("the jwks_uri is not on the client_id's host".into());
        }
        let redirect_uris: Vec<String> = doc["redirect_uris"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| u.as_str().map(str::to_string))
            .collect();
        if redirect_uris.iter().any(|u| host(u) != home) {
            return Err("a redirect_uri is not on the client_id's host".into());
        }
        let jwks = self.fetch(jwks_uri)?;
        let keys: Vec<Value> = jwks["keys"].as_array().cloned().unwrap_or_default();
        if keys
            .iter()
            .any(|k| PRIVATE_MEMBERS.iter().any(|m| k.get(*m).is_some()))
        {
            return Err("the key set holds a private key".into());
        }
        let client = Arc::new(Client {
            keys,
            redirect_uris,
        });
        lock(&self.clients).insert(client_id.to_string(), Arc::clone(&client));
        Ok(client)
    }

    /// Verify a JWT `jws` against `client`'s key set: by its `kid`, or by any
    /// key when it names none.
    fn signed_by(client: &Client, jws: &Jws) -> bool {
        jws.header["alg"] == wire::ES256
            && client
                .keys
                .iter()
                .filter(|k| jws.header.get("kid").is_none_or(|kid| k["kid"] == *kid))
                .any(|k| verify_es256(k, jws))
    }

    /// The claims every assertion carries: `aud` the issuer as one string, a
    /// short life around now, and a `jti` of at least 128 bits not seen
    /// before (§4.1, §4.2). `seen` is the replay cache it is recorded in.
    fn check_claims(
        &self,
        claims: &Value,
        client_id: &str,
        seen: &mut BTreeMap<String, u64>,
    ) -> Result<(), String> {
        let t = now();
        match &claims["aud"] {
            Value::String(a) if *a == self.issuer() => {}
            Value::String(a) if *a == format!("{}/token", self.endpoints()) => {
                return Err("aud names the token endpoint, not the issuer".into());
            }
            Value::Array(_) => return Err("aud is a list, not one string".into()),
            _ => return Err("aud is not the Company's issuer".into()),
        }
        let (Some(iat), Some(exp)) = (claims["iat"].as_u64(), claims["exp"].as_u64()) else {
            return Err("iat and exp must be integers".into());
        };
        if exp + WINDOW_S / 2 < t || iat > t + WINDOW_S / 2 {
            return Err("the assertion is expired, or issued in the future".into());
        }
        if exp <= iat || exp - iat > MAX_ASSERTION_S {
            return Err(format!(
                "the assertion lives {}s; the fake takes at most {MAX_ASSERTION_S}",
                exp.saturating_sub(iat)
            ));
        }
        let jti = claims["jti"].as_str().unwrap_or_default();
        if jti.len() < 22 {
            return Err("the jti has fewer than 128 bits".into());
        }
        seen.retain(|_, until| *until >= t);
        if seen
            .insert(format!("{client_id} {jti}"), exp + WINDOW_S)
            .is_some()
        {
            return Err("the jti was used before".into());
        }
        Ok(())
    }

    /// The client authentication of every token, device and revocation
    /// request: `private_key_jwt` with the `client_id` the form names.
    fn authenticate(
        &self,
        form: &BTreeMap<String, String>,
    ) -> Result<(String, Arc<Client>), Refusal> {
        let invalid = |why: String| refuse(401, err::INVALID_CLIENT, why);
        let client_id = form
            .get("client_id")
            .ok_or_else(|| invalid("no client_id".into()))?;
        if form.get("client_assertion_type").map(String::as_str) != Some(ASSERTION_TYPE) {
            return Err(invalid(
                "client_assertion_type is not the JWT bearer type".into(),
            ));
        }
        let assertion = form
            .get("client_assertion")
            .ok_or_else(|| invalid("no client_assertion".into()))?;
        let jws =
            decode_jws(assertion).map_err(|e| invalid(format!("the client assertion: {e}")))?;
        seen("client_assertion", jws.claims.clone());
        let client = self
            .client(client_id, jws.header["kid"].as_str())
            .map_err(invalid)?;
        if !Company::signed_by(&client, &jws) {
            return Err(invalid(
                "the client assertion is not signed ES256 by a key in the client's jwks_uri".into(),
            ));
        }
        if jws.claims["iss"] != **client_id || jws.claims["sub"] != **client_id {
            return Err(invalid(
                "the client assertion's iss and sub are not the client_id".into(),
            ));
        }
        let mut st = lock(&self.state);
        self.check_claims(&jws.claims, client_id, &mut st.client_jtis)
            .map_err(|e| invalid(format!("the client assertion: {e}")))?;
        Ok((client_id.clone(), client))
    }

    // --- DPoP (§4.3) ---------------------------------------------------

    /// Validate the request's DPoP Proof as §4.3 lists, if it carries one:
    /// `token` is the Session Token sent with it, and `token_endpoint` says
    /// the proof is to the token endpoint, which must carry no `ath` and
    /// never a `jti` it accepted before.
    fn check_proof(
        &self,
        req: &Request,
        token: Option<&str>,
        token_endpoint: bool,
        st: &mut State,
    ) -> Result<Option<Proof>, ProofError> {
        let bad = |why: &str| ProofError::Invalid(why.to_string());
        let proofs = req.all("dpop");
        let proof = match proofs.as_slice() {
            [] => return Ok(None),
            [one] => *one,
            more => {
                return Err(ProofError::Invalid(format!(
                    "the request has {} DPoP headers",
                    more.len()
                )));
            }
        };
        let jws = decode_jws(proof).map_err(|e| ProofError::Invalid(format!("the proof: {e}")))?;
        seen("proof", jws.claims.clone());
        let parts = json!({"header": jws.header, "payload": jws.claims});
        kit::kit()
            .validate("dpop-proof.schema.json", &parts)
            .map_err(|e| ProofError::Invalid(format!("the proof: {}", e.join("; "))))?;
        validated("dpop-proof.schema.json");
        if jws.header["alg"] != wire::ES256 {
            return Err(bad(
                "the proof's alg is not one the Company accepts (ES256)",
            ));
        }
        let key = &jws.header["jwk"];
        if !verify_es256(key, &jws) {
            return Err(bad(
                "the proof's signature does not verify with the key in its header",
            ));
        }
        let claims = &jws.claims;
        if claims["htm"] != *req.method {
            return Err(bad("htm is not the request's method"));
        }
        if claims["htu"] != *req.htu() {
            return Err(bad("htu is not the request's URL"));
        }
        let t = now();
        let iat = claims["iat"].as_u64().unwrap_or_default();
        if iat + WINDOW_S < t || iat > t + WINDOW_S {
            return Err(bad("the proof was not made within the acceptance window"));
        }
        if self.has(Variant::NonceRequired) {
            st.nonces.retain(|_, at| *at + NONCE_S >= t);
            let fresh = claims["nonce"]
                .as_str()
                .is_some_and(|n| st.nonces.contains_key(n));
            if !fresh {
                let n = opaque("nonce_");
                st.nonces.insert(n.clone(), t);
                return Err(ProofError::Nonce(n));
            }
        }
        match (token_endpoint, token, claims["ath"].as_str()) {
            (true, _, Some(_)) => return Err(bad("a proof to the token endpoint carries no ath")),
            (false, Some(tok), ath) => {
                let want = b64::url_nopad(&crate::sha::sha256(tok.as_bytes()));
                if ath != Some(want.as_str()) {
                    return Err(bad("ath is not the hash of the Session Token sent with it"));
                }
            }
            _ => {}
        }
        if token_endpoint {
            let jti = claims["jti"].as_str().unwrap_or_default().to_string();
            st.proof_jtis.retain(|_, at| *at + 2 * WINDOW_S >= t);
            if st.proof_jtis.insert(jti, t).is_some() {
                return Err(bad("the proof's jti was accepted before"));
            }
        }
        let jkt = jwk::thumbprint(key)
            .map_err(|e| ProofError::Invalid(format!("the proof's jwk: {e}")))?;
        seen("jkt", json!(jkt));
        Ok(Some(Proof { jkt }))
    }

    /// A `RetryAfter` refusal for the endpoint `class`, if one is due.
    fn rate_limited(&self, class: &'static str, st: &mut State) -> Option<bool> {
        if !self.has(Variant::RetryAfter) {
            return None;
        }
        let limit = st.limits.entry(class).or_default();
        let at = Instant::now();
        match limit.until {
            Some(until) if at < until => Some(true),
            _ if !limit.tripped => {
                limit.tripped = true;
                limit.until = Some(at + Duration::from_secs(RETRY_AFTER_S));
                Some(false)
            }
            _ => None,
        }
    }

    // --- The token endpoint (§4.2–§4.8) --------------------------------

    fn token(&self, req: &Request) -> Reply {
        let early = self.rate_limited("token", &mut lock(&self.state));
        if let Some(early) = early {
            return self
                .oauth_error(refuse(
                    429,
                    err::RATE_LIMITED,
                    "too many requests for this client_id",
                ))
                .note("early", json!(early));
        }
        let form = match decode_form(&req.body) {
            Ok(f) => f,
            Err(e) => return self.oauth_error(refuse(400, err::INVALID_REQUEST, e)),
        };
        let g = form.get("grant_type").cloned().unwrap_or_default();
        let resource = form.get(wire::param::RESOURCE).cloned();
        let reply = match self.token_inner(req, &form, &g) {
            Ok(r) => r,
            Err(r) => self.oauth_error(r),
        };
        reply
            .note("grant", json!(g))
            .note("dpop", json!(req.header("dpop").is_some()))
            .note("resource", json!(resource))
            .note("session_id", json!(form.get(wire::param::SESSION_ID)))
    }

    fn token_inner(
        &self,
        req: &Request,
        form: &BTreeMap<String, String>,
        g: &str,
    ) -> Result<Reply, Refusal> {
        {
            let mut st = lock(&self.state);
            if st.refuse_grants > 0 {
                st.refuse_grants -= 1;
                return Err(refuse(
                    401,
                    err::INVALID_CLIENT,
                    "this client is not accepted here",
                ));
            }
        }
        let (client_id, client) = self.authenticate(form)?;
        let mut st = lock(&self.state);
        let proof = self
            .check_proof(req, None, true, &mut st)
            .map_err(proof_refusal)?;
        let resource = form.get(wire::param::RESOURCE);
        let binding = match (proof, resource) {
            (Some(_), Some(r)) if self.mcp_urls().contains(r) => {
                return Err(refuse(
                    400,
                    err::INVALID_REQUEST,
                    "a token for an MCP server is requested with a DPoP Proof; MCP takes a Bearer token, requested without one",
                ));
            }
            (Some(_), Some(r)) if *r != self.api_url() => {
                return Err(refuse(
                    400,
                    err::INVALID_TARGET,
                    "resource names nothing this Company lists",
                ));
            }
            (Some(p), _) => Binding::Dpop(p.jkt),
            (None, Some(r)) if self.mcp_urls().contains(r) => Binding::Bearer(r.clone()),
            (None, Some(_)) => {
                return Err(refuse(
                    400,
                    err::INVALID_TARGET,
                    "a Bearer token is only for a listed MCP server",
                ));
            }
            (None, None) => {
                return Err(refuse(
                    400,
                    err::INVALID_DPOP_PROOF,
                    "no DPoP Proof, and no resource naming an MCP server for a Bearer token",
                ));
            }
        };
        let session_id = form.get(wire::param::SESSION_ID).map(String::as_str);
        let granted = match g {
            grant::JWT_BEARER => {
                // The assertion carries no account approval: a token it gets
                // is signed out, whatever the Session is (§4.2, §4.8), and
                // holds no scope a request could narrow it to.
                let scope = Company::narrowed(form, &[])?;
                Granted {
                    session_id: self.jwt_bearer(form, &client_id, &client, session_id, &mut st)?,
                    scope,
                    account_token: None,
                }
            }
            grant::AUTHORIZATION_CODE => self.code_grant(form, &client_id, session_id, &mut st)?,
            grant::DEVICE_CODE => self.device_grant(form, &client_id, session_id, &mut st)?,
            grant::REFRESH_TOKEN => self.refresh_grant(form, &client_id, session_id, &mut st)?,
            _ => {
                return Err(refuse(
                    400,
                    err::UNSUPPORTED_GRANT_TYPE,
                    "not a grant this Company has",
                ));
            }
        };
        let Granted {
            session_id,
            scope,
            account_token: issued_refresh,
        } = granted;
        let signed_in = g != grant::JWT_BEARER && st.sessions[&session_id].signed_in;
        let scope = scope.join(" ");
        let token_type = match binding {
            Binding::Dpop(_) => wire::TOKEN_TYPE_DPOP,
            Binding::Bearer(_) => wire::TOKEN_TYPE_BEARER,
        };
        let access_token = opaque("st_");
        let expires_in = st.expires_in;
        let mut body = json!({
            "access_token": access_token,
            "token_type": token_type,
            "expires_in": expires_in,
            "scope": scope,
            wire::member::SESSION_ID: session_id,
            wire::member::SIGNED_IN: signed_in,
        });
        // The Account Token goes out once: when a sign-in issues it, and when
        // a rotation replaces it.
        if let Some(rt) = &issued_refresh {
            body["refresh_token"] = json!(rt);
            body["refresh_token_expires_in"] = json!(2_592_000);
        }
        let body = match self.conformant("token-response.schema.json", body) {
            Ok(b) => b,
            Err(drift) => return Ok(drift),
        };
        st.issued += 1;
        let seq = st.issued;
        st.access.insert(
            access_token.clone(),
            Access {
                session_id,
                binding,
                signed_in,
                scope,
                expires_at: now() + expires_in,
                seq,
            },
        );
        Ok(Reply::json(200, &body)
            .with("Cache-Control", "no-store")
            .note(
                "issued",
                json!({"access_token": access_token, "refresh_token": issued_refresh}),
            ))
    }

    /// The scopes a token gets of `has`, those its grant holds: a token
    /// request's `scope` may ask for fewer, never for more (§4.3, §4.8), so
    /// the Company grants nothing beyond what was requested and allowed
    /// (§4.4). A request without `scope` gets all of `has`.
    fn narrowed(form: &BTreeMap<String, String>, has: &[String]) -> Result<Vec<String>, Refusal> {
        let Some(asked) = form.get("scope") else {
            return Ok(has.to_vec());
        };
        let mut scope: Vec<String> = Vec::new();
        for s in asked.split(' ').filter(|s| !s.is_empty()) {
            if !has.iter().any(|h| h == s) {
                return Err(refuse(
                    400,
                    err::INVALID_SCOPE,
                    format!("scope asks for {s}, which this grant does not hold"),
                ));
            }
            if !scope.iter().any(|x| x == s) {
                scope.push(s.to_string());
            }
        }
        Ok(scope)
    }

    /// Start or renew a Session with the Personal Agent's Session assertion
    /// (§4.2): the Session id the token is for.
    fn jwt_bearer(
        &self,
        form: &BTreeMap<String, String>,
        client_id: &str,
        client: &Client,
        session_id: Option<&str>,
        st: &mut State,
    ) -> Result<String, Refusal> {
        let invalid = |why: String| refuse(400, err::INVALID_GRANT, why);
        let assertion = form
            .get("assertion")
            .ok_or_else(|| invalid("no assertion".into()))?;
        let jws =
            decode_jws(assertion).map_err(|e| invalid(format!("the Session assertion: {e}")))?;
        seen("assertion", jws.claims.clone());
        if !Company::signed_by(client, &jws) {
            return Err(invalid(
                "the Session assertion is not signed ES256 by a key in the client's jwks_uri"
                    .into(),
            ));
        }
        kit::kit()
            .validate("session-assertion.schema.json", &jws.claims)
            .map_err(|e| invalid(format!("the Session assertion: {}", e.join("; "))))?;
        validated("session-assertion.schema.json");
        if jws.claims["iss"] != *client_id {
            return Err(invalid(
                "the Session assertion's iss is not the authenticated client_id".into(),
            ));
        }
        self.check_claims(&jws.claims, client_id, &mut st.assertion_jtis)
            .map_err(|e| invalid(format!("the Session assertion: {e}")))?;
        let user_id = jws.claims["sub"].as_str().unwrap_or_default().to_string();
        match session_id {
            None => {
                let id = opaque("ses_");
                st.sessions.insert(
                    id.clone(),
                    Session {
                        client_id: client_id.to_string(),
                        user_id,
                        signed_in: false,
                        account: None,
                        scope: String::new(),
                        grant: None,
                    },
                );
                Ok(id)
            }
            Some(id) => match st.sessions.get(id) {
                Some(s) if s.client_id == client_id && s.user_id == user_id => Ok(id.to_string()),
                _ => Err(refuse(
                    400,
                    err::INVALID_SESSION,
                    "the Session has ended, or is another Personal Agent's or User's",
                )),
            },
        }
    }

    /// The scopes a sign-in grants: what the scripted User approves of what
    /// was asked for.
    fn granted(&self, asked: &[String], st: &State) -> Vec<String> {
        match &st.approver.scope {
            Some(only) => asked.iter().filter(|s| only.contains(s)).cloned().collect(),
            None => asked.to_vec(),
        }
    }

    /// Sign the Session `session_id` in to `account` with `scope`, unless it
    /// was ever signed in to another (§4.4): the Session id, and the Account
    /// Token the sign-in issues.
    fn sign_in(
        &self,
        client_id: &str,
        session_id: Option<&str>,
        account: &str,
        scope: Vec<String>,
        st: &mut State,
    ) -> Result<(String, String), Refusal> {
        let id = session_id.ok_or_else(|| refuse(400, err::INVALID_REQUEST, "no session_id"))?;
        let s = match st.sessions.get_mut(id) {
            Some(s) if s.client_id == client_id => s,
            _ => {
                return Err(refuse(
                    400,
                    err::INVALID_SESSION,
                    "the Session has ended, or is another Personal Agent's",
                ));
            }
        };
        if s.account.as_deref().is_some_and(|a| a != account) {
            return Err(refuse(
                400,
                err::ACCOUNT_MISMATCH,
                "the Session is, or was, signed in to a different account",
            ));
        }
        let (rt, grant) = (opaque("pat_"), opaque("grant_"));
        s.signed_in = true;
        s.account = Some(account.to_string());
        s.scope = scope.join(" ");
        s.grant = Some(grant.clone());
        let user_id = s.user_id.clone();
        st.accounts.insert(
            rt.clone(),
            Account {
                client_id: client_id.to_string(),
                user_id,
                account: account.to_string(),
                scope,
                grant,
                revoked: false,
                spent: false,
            },
        );
        Ok((id.to_string(), rt))
    }

    fn code_grant(
        &self,
        form: &BTreeMap<String, String>,
        client_id: &str,
        session_id: Option<&str>,
        st: &mut State,
    ) -> Result<Granted, Refusal> {
        let need = |k: &str| {
            form.get(k)
                .cloned()
                .ok_or_else(|| refuse(400, err::INVALID_REQUEST, format!("no {k}")))
        };
        let (code, redirect_uri, verifier) =
            (need("code")?, need("redirect_uri")?, need("code_verifier")?);
        let invalid = |why: &str| refuse(400, err::INVALID_GRANT, why);
        let c = st
            .codes
            .get_mut(&code)
            .ok_or_else(|| invalid("an unknown code"))?;
        if c.used || c.client_id != client_id || c.expires_at < now() {
            return Err(invalid("the code is spent, expired, or another client's"));
        }
        c.used = true;
        if c.redirect_uri != redirect_uri {
            return Err(invalid(
                "redirect_uri is not the one the code was issued for",
            ));
        }
        if b64::url_nopad(&crate::sha::sha256(verifier.as_bytes())) != c.challenge {
            return Err(invalid(
                "the code_verifier does not match the code_challenge",
            ));
        }
        let (account, approved) = (c.account.clone(), c.scope.clone());
        let scope = Company::narrowed(form, &approved)?;
        let (session_id, rt) = self.sign_in(client_id, session_id, &account, approved, st)?;
        Ok(Granted {
            session_id,
            scope,
            account_token: Some(rt),
        })
    }

    fn device_grant(
        &self,
        form: &BTreeMap<String, String>,
        client_id: &str,
        session_id: Option<&str>,
        st: &mut State,
    ) -> Result<Granted, Refusal> {
        let code = form
            .get("device_code")
            .ok_or_else(|| refuse(400, err::INVALID_REQUEST, "no device_code"))?;
        let decision = st.approver.decision;
        let after = st.approver.after_polls;
        let account = st.approver.account.clone();
        let d = match st.devices.get_mut(code) {
            Some(d) if d.client_id == client_id && !d.done => d,
            _ => {
                return Err(refuse(
                    400,
                    err::INVALID_GRANT,
                    "an unknown, spent or another client's device_code",
                ));
            }
        };
        if d.expires_at < now() {
            d.done = true;
            return Err(refuse(
                400,
                err::EXPIRED_TOKEN,
                "the device code has expired",
            ));
        }
        let at = Instant::now();
        let early = d.last_poll.is_some_and(|last| {
            at.duration_since(last) + Duration::from_millis(250) < Duration::from_secs(d.interval)
        });
        d.last_poll = Some(at);
        if early {
            d.interval += 5;
            return Err(refuse(
                400,
                err::SLOW_DOWN,
                "polled faster than the interval",
            ));
        }
        d.polls += 1;
        if d.polls <= after {
            return Err(refuse(
                400,
                err::AUTHORIZATION_PENDING,
                "the User has not finished",
            ));
        }
        let asked = d.scope.clone();
        match decision {
            Decision::Deny => {
                d.done = true;
                return Err(refuse(400, err::ACCESS_DENIED, "the User declined"));
            }
            Decision::Expire => {
                d.done = true;
                return Err(refuse(
                    400,
                    err::EXPIRED_TOKEN,
                    "the User did not finish in time",
                ));
            }
            Decision::Approve => {}
        }
        // A poll asking for more than the User approved is refused, and
        // leaves the code for the poll that asks for no more.
        let approved = self.granted(&asked, st);
        let scope = Company::narrowed(form, &approved)?;
        if let Some(d) = st.devices.get_mut(code) {
            d.done = true;
        }
        let (session_id, rt) = self.sign_in(client_id, session_id, &account, approved, st)?;
        Ok(Granted {
            session_id,
            scope,
            account_token: Some(rt),
        })
    }

    /// Use an Account Token (§4.8), with the rotated Account Token when
    /// [`Variant::Rotation`] replaces it.
    fn refresh_grant(
        &self,
        form: &BTreeMap<String, String>,
        client_id: &str,
        session_id: Option<&str>,
        st: &mut State,
    ) -> Result<Granted, Refusal> {
        let rt = form
            .get("refresh_token")
            .ok_or_else(|| refuse(400, err::INVALID_REQUEST, "no refresh_token"))?;
        let account = match st.accounts.get(rt) {
            Some(a) if !a.revoked && !a.spent && a.client_id == client_id => a.clone(),
            _ => {
                return Err(refuse(
                    400,
                    err::INVALID_GRANT,
                    "the Account Token is unknown, revoked, spent or another client's",
                ));
            }
        };
        let scope = Company::narrowed(form, &account.scope)?;
        let id = match session_id {
            Some(id) => {
                let s = match st.sessions.get(id) {
                    Some(s) if s.client_id == client_id && s.user_id == account.user_id => s,
                    _ => {
                        return Err(refuse(
                            400,
                            err::INVALID_SESSION,
                            "the Session has ended, or is another Personal Agent's or User's",
                        ));
                    }
                };
                if s.account.as_deref().is_some_and(|a| a != account.account) {
                    return Err(refuse(
                        400,
                        err::ACCOUNT_MISMATCH,
                        "the Session is, or was, signed in to a different account",
                    ));
                }
                id.to_string()
            }
            None => {
                let id = opaque("ses_");
                st.sessions.insert(
                    id.clone(),
                    Session {
                        client_id: client_id.to_string(),
                        user_id: account.user_id.clone(),
                        signed_in: false,
                        account: None,
                        scope: String::new(),
                        grant: None,
                    },
                );
                id
            }
        };
        let rotated = if self.has(Variant::Rotation) {
            let next = opaque("pat_");
            if let Some(old) = st.accounts.get_mut(rt) {
                old.spent = true;
            }
            st.accounts.insert(next.clone(), account.clone());
            Some(next)
        } else {
            None
        };
        if let Some(s) = st.sessions.get_mut(&id) {
            s.signed_in = true;
            s.account = Some(account.account.clone());
            s.scope = account.scope.join(" ");
            s.grant = Some(account.grant.clone());
        }
        Ok(Granted {
            session_id: id,
            scope,
            account_token: rotated,
        })
    }

    // --- Device and Direct Sign-In (§4.5, §4.6) ------------------------

    /// The scopes a sign-in request asks for: present, and each one the
    /// sign-in type can grant (§4.4).
    fn asked_scopes(scope: Option<&String>) -> Result<Vec<String>, &'static str> {
        let Some(scope) = scope else {
            return Err("a sign-in request must include scope");
        };
        let asked: Vec<String> = scope
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if asked.is_empty() || asked.iter().any(|s| !SIGN_IN_SCOPES.contains(&s.as_str())) {
            return Err("scope asks for one this sign-in type cannot grant");
        }
        Ok(asked)
    }

    fn device_authorization(&self, req: &Request) -> Reply {
        let form = match decode_form(&req.body) {
            Ok(f) => f,
            Err(e) => return self.oauth_error(refuse(400, err::INVALID_REQUEST, e)),
        };
        let reply = (|| -> Result<Reply, Refusal> {
            let (client_id, _) = self.authenticate(&form)?;
            let mut st = lock(&self.state);
            self.check_proof(req, None, false, &mut st)
                .map_err(proof_refusal)?;
            let asked = Company::asked_scopes(form.get("scope"))
                .map_err(|why| refuse(400, err::INVALID_SCOPE, why))?;
            let device_code = opaque("dev_");
            let user_code = user_code();
            let at = if self.has(Variant::OffHostVerificationUri) {
                format!("https://localhost:{}/auth/device/verify", self.port)
            } else {
                format!("{}/device/verify", self.issuer())
            };
            let body = json!({
                "device_code": device_code,
                "user_code": user_code,
                "verification_uri": at,
                "verification_uri_complete": format!("{at}?{}", encode_query(&[("user_code", &user_code)])),
                "expires_in": DEVICE_EXPIRES_S,
                "interval": DEVICE_INTERVAL_S,
            });
            let body = match self.conformant("device-authorization-response.schema.json", body) {
                Ok(b) => b,
                Err(drift) => return Ok(drift),
            };
            st.devices.insert(
                device_code,
                Device {
                    client_id,
                    scope: asked,
                    expires_at: now() + DEVICE_EXPIRES_S,
                    interval: DEVICE_INTERVAL_S,
                    polls: 0,
                    last_poll: None,
                    done: false,
                },
            );
            Ok(Reply::json(200, &body).with("Cache-Control", "no-store"))
        })();
        reply.unwrap_or_else(|r| self.oauth_error(r))
    }

    /// Direct Sign-In's authorization request, answered at once by the
    /// scripted User: a redirect to the Personal Agent's `redirect_uri` with
    /// a one-time `code`, the `state` and `iss` (§4.5, RFC 9207), or
    /// `access_denied`. A `redirect_uri` the client does not list gets no
    /// redirect at all.
    fn authorize(&self, req: &Request) -> Reply {
        let q = match decode_form(req.query.as_bytes()) {
            Ok(q) => q,
            Err(e) => return self.oauth_error(refuse(400, err::INVALID_REQUEST, e)),
        };
        let page = |why: &str| self.oauth_error(refuse(400, err::INVALID_REQUEST, why));
        let Some(client_id) = q.get("client_id") else {
            return page("no client_id");
        };
        let client = match self.client(client_id, None) {
            Ok(c) => c,
            Err(e) => return self.oauth_error(refuse(400, err::INVALID_CLIENT, e)),
        };
        let Some(redirect_uri) = q
            .get("redirect_uri")
            .filter(|u| client.redirect_uris.contains(u))
        else {
            return page("redirect_uri does not exactly match one of the client's redirect_uris");
        };
        let state = q.get("state").map(String::as_str).unwrap_or_default();
        let issuer = self.issuer();
        let back = |pairs: &[(&str, &str)]| {
            let sep = if redirect_uri.contains('?') { '&' } else { '?' };
            Reply::new(302).with(
                "Location",
                &format!("{redirect_uri}{sep}{}", encode_query(pairs)),
            )
        };
        let refused = |code: &str| {
            back(&[("error", code), ("state", state), ("iss", &issuer)]).note("error", json!(code))
        };
        if q.get("response_type").map(String::as_str) != Some("code")
            || q.get("code_challenge_method").map(String::as_str) != Some("S256")
            || !q.contains_key("code_challenge")
            || state.is_empty()
        {
            return refused(err::INVALID_REQUEST);
        }
        let asked = match Company::asked_scopes(q.get("scope")) {
            Ok(a) => a,
            Err(_) => return refused(err::INVALID_SCOPE),
        };
        let mut st = lock(&self.state);
        if st.approver.decision != Decision::Approve {
            return refused(err::ACCESS_DENIED);
        }
        let code = opaque("code_");
        let scope = self.granted(&asked, &st);
        let account = st.approver.account.clone();
        st.codes.insert(
            code.clone(),
            Code {
                client_id: client_id.clone(),
                redirect_uri: redirect_uri.clone(),
                challenge: q.get("code_challenge").cloned().unwrap_or_default(),
                scope,
                account,
                used: false,
                expires_at: now() + 300,
            },
        );
        back(&[("code", &code), ("state", state), ("iss", &issuer)])
    }

    /// RFC 7009 revocation of an Account Token (§4.9): the sign-in it belongs
    /// to stops working, with every Account Token a rotation issued for it,
    /// and every Session signed in with any of them continues signed out. A
    /// spent token revokes nothing, and the record says it was spent.
    fn revoke(&self, req: &Request) -> Reply {
        let form = match decode_form(&req.body) {
            Ok(f) => f,
            Err(e) => return self.oauth_error(refuse(400, err::INVALID_REQUEST, e)),
        };
        let reply = (|| -> Result<Reply, Refusal> {
            let (client_id, _) = self.authenticate(&form)?;
            let mut st = lock(&self.state);
            self.check_proof(req, None, false, &mut st)
                .map_err(proof_refusal)?;
            let token = form
                .get("token")
                .ok_or_else(|| refuse(400, err::INVALID_REQUEST, "no token"))?;
            // A token that is unknown, another client's, already revoked or
            // spent is not revoked, and the answer is the same (RFC 7009
            // §2.2).
            let (grant, spent) = match st.accounts.get(token) {
                Some(a) if a.client_id == client_id && !a.revoked && !a.spent => {
                    (Some(a.grant.clone()), false)
                }
                Some(a) if a.client_id == client_id => (None, a.spent),
                _ => (None, false),
            };
            if let Some(grant) = &grant {
                for a in st.accounts.values_mut() {
                    if a.grant == *grant {
                        a.revoked = true;
                    }
                }
                for s in st.sessions.values_mut() {
                    if s.grant.as_ref() == Some(grant) {
                        s.signed_in = false;
                        s.scope.clear();
                        s.grant = None;
                    }
                }
            }
            Ok(Reply::new(200)
                .note("revoked", json!(grant.is_some()))
                .note("spent", json!(spent)))
        })();
        reply.unwrap_or_else(|r| self.oauth_error(r))
    }

    // --- Using Session Tokens: the API and MCP (§4.3, §6) --------------

    /// Whether the token's Session is signed in now: a token issued signed in
    /// stops counting as such once its Account Token is revoked, since the
    /// fake checks on every request (§4.9).
    fn signed_in_now(access: &Access, st: &State) -> bool {
        access.signed_in
            && st
                .sessions
                .get(&access.session_id)
                .is_some_and(|s| s.signed_in)
    }

    /// The API's one operation, which takes a DPoP Session Token and its
    /// proof, and nothing else.
    fn session_operation(&self, req: &Request) -> Reply {
        let challenge = |error: &str, why: &str| {
            Reply::new(401)
                .with(
                    "WWW-Authenticate",
                    &format!(
                        "DPoP error=\"{error}\", error_description=\"{why}\", algs=\"{}\"",
                        wire::ES256
                    ),
                )
                .note("error", json!(error))
        };
        if req.query.contains("access_token=") {
            return self.oauth_error(refuse(
                400,
                err::INVALID_REQUEST,
                "a token in a URL is refused",
            ));
        }
        let Some(auth) = req.header("authorization") else {
            return Reply::new(401).with(
                "WWW-Authenticate",
                &format!("DPoP algs=\"{}\"", wire::ES256),
            );
        };
        let Some(token) = auth.strip_prefix("DPoP ") else {
            return challenge(
                err::INVALID_TOKEN,
                "this API takes DPoP Session Tokens only",
            );
        };
        let mut st = lock(&self.state);
        let jkt = match st.access.get(token) {
            Some(a) if a.expires_at >= now() => match &a.binding {
                Binding::Dpop(jkt) => jkt.clone(),
                Binding::Bearer(_) => {
                    return challenge(
                        err::INVALID_TOKEN,
                        "a Bearer token is for its MCP server only",
                    );
                }
            },
            _ => return challenge(err::INVALID_TOKEN, "the token is unknown or expired"),
        };
        let proof = match self.check_proof(req, Some(token), false, &mut st) {
            Ok(Some(p)) => p,
            Ok(None) => return challenge(err::INVALID_DPOP_PROOF, "no DPoP Proof"),
            Err(ProofError::Invalid(why)) => return challenge(err::INVALID_DPOP_PROOF, &why),
            Err(ProofError::Nonce(n)) => {
                return challenge(err::USE_DPOP_NONCE, "a nonce is required")
                    .with(wire::DPOP_NONCE_HEADER, &n);
            }
        };
        if proof.jkt != jkt {
            return challenge(
                err::INVALID_DPOP_PROOF,
                "the proof's key is not the one the token is bound to",
            );
        }
        let a = &st.access[token];
        Reply::json(
            200,
            &json!({"session_id": a.session_id, "signed_in": Company::signed_in_now(a, &st), "scope": a.scope}),
        )
    }

    /// An MCP server (§6): Streamable HTTP over Bearer Session Tokens issued
    /// for its URL. It answers `server/discover` (MCP 2026-07-28) and the
    /// `initialize` handshake, or with [`Variant::Mcp20250618`] only the
    /// handshake, at 2025-06-18.
    fn mcp(&self, req: &Request) -> Reply {
        let url = format!("{}{}", self.origin(), req.path);
        let metadata = format!(
            "{}/.well-known/{}{}",
            self.origin(),
            wire::RESOURCE_METADATA_SUFFIX,
            req.path
        );
        let challenge = |status: u16, error: Option<&str>| {
            let mut v = format!("Bearer resource_metadata=\"{metadata}\"");
            if let Some(e) = error {
                v = format!("Bearer error=\"{e}\", resource_metadata=\"{metadata}\"");
            }
            Reply::new(status)
                .with("WWW-Authenticate", &v)
                .note("error", json!(error))
        };
        if req.query.contains("access_token=") {
            return self.oauth_error(refuse(
                400,
                err::INVALID_REQUEST,
                "a token in a URL is refused",
            ));
        }
        let Some(auth) = req.header("authorization") else {
            return challenge(401, None);
        };
        let Some(token) = auth.strip_prefix("Bearer ") else {
            return challenge(401, Some(err::INVALID_TOKEN));
        };
        if req.method != "POST" {
            return Reply::new(405);
        }
        let mut st = lock(&self.state);
        if let Some(ext) = st.extension_required.clone() {
            return Reply::new(403)
                .with(
                    "WWW-Authenticate",
                    &format!(
                        "Bearer error=\"{}\", extension=\"{ext}\"",
                        err::EXTENSION_REQUIRED
                    ),
                )
                .note("error", json!(err::EXTENSION_REQUIRED));
        }
        if st.refuse_tokens > 0 {
            st.refuse_tokens -= 1;
            return challenge(401, Some(err::INVALID_TOKEN));
        }
        let (signed_in, scope) = match st.access.get(token) {
            Some(a) if a.expires_at >= now() && a.binding == Binding::Bearer(url.clone()) => {
                (Company::signed_in_now(a, &st), a.scope.clone())
            }
            _ => return challenge(401, Some(err::INVALID_TOKEN)),
        };
        let Ok(frame) = serde_json::from_slice::<Value>(&req.body) else {
            return Reply::json(
                400,
                &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
            );
        };
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        let Some(id) = frame.get("id").cloned() else {
            return Reply::new(202).note("mcp", json!(method));
        };
        let ok = |result: Value| {
            Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
        };
        let rpc_err = |code: i64, message: &str| {
            Reply::json(
                200,
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
            )
        };
        let old = self.has(Variant::Mcp20250618);
        // What a request says about its caller beside its arguments: the
        // keys of its `_meta`, the trace it names, the title its client
        // gives itself (in `initialize`'s `clientInfo`, or, at 2026-07-28,
        // on every request).
        let m = &frame["params"]["_meta"];
        let meta: Vec<String> = m
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let trace_id = m["traceparent"]
            .as_str()
            .and_then(|t| t.split('-').nth(1))
            .map(str::to_string);
        let title = frame["params"]["clientInfo"]["title"]
            .as_str()
            .or_else(|| m["io.modelcontextprotocol/clientInfo"]["title"].as_str())
            .map(str::to_string);
        let mcp_session = req.header("mcp-session-id").map(str::to_string);
        let held = st
            .holds
            .iter_mut()
            .find(|h| h.method == method && h.times > 0)
            .map(|h| {
                h.times -= 1;
                (h.ms, h.status, h.body.clone())
            });
        let stalled = held.as_ref().is_some_and(|(_, status, _)| status.is_none());
        if let Some((ms, status, body)) = held {
            drop(st);
            std::thread::sleep(Duration::from_millis(ms));
            if let Some(code) = status {
                let reply = match body {
                    Some(mut b) => {
                        b["jsonrpc"] = json!("2.0");
                        b["id"] = id.clone();
                        Reply::json(code, &b)
                    }
                    None => Reply::new(code),
                };
                return reply
                    .note("mcp", json!(method))
                    .note("meta", json!(meta))
                    .note("trace_id", json!(trace_id))
                    .note("mcp_session", json!(mcp_session))
                    .note("failed", json!(code));
            }
            st = lock(&self.state);
        }
        let reply = match method.as_str() {
            "server/discover" if !old => ok(json!({
                "resultType": "complete",
                "supportedVersions": [::mcp::wire::DISCOVER_PROTOCOL_VERSION],
                "capabilities": {"tools": {}},
                "ttlMs": 0,
                "cacheScope": "private",
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fake-company", "version": crate::VERSION}}
            })),
            "initialize" => {
                let asked = frame["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or_default();
                let speaks = if old {
                    wire::MCP_MIN_VERSION
                } else if asked == wire::MCP_MIN_VERSION {
                    asked
                } else {
                    ::mcp::wire::PROTOCOL_VERSION
                };
                ok(json!({
                    "protocolVersion": speaks,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-company", "version": crate::VERSION}
                }))
                .with("Mcp-Session-Id", &opaque("mcp_"))
            }
            "ping" => ok(json!({})),
            "tools/list" => ok(json!({"tools": [
                {"name": "search", "description": "Search the catalogue.",
                 "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
                {"name": "order_status", "description": format!("The status of one of the User's orders. Needs a signed-in Session with {}.", wire::SCOPE_READ),
                 "inputSchema": {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]}}
            ]})),
            "tools/call" => {
                if let Some(early) = self.rate_limited("mcp", &mut st) {
                    return Reply::new(429)
                        .with("Retry-After", &RETRY_AFTER_S.to_string())
                        .note("early", json!(early))
                        .note("mcp", json!(method))
                        .note("meta", json!(meta));
                }
                let args = &frame["params"]["arguments"];
                let key = m["agent/idempotency_key"].clone();
                let answer = match frame["params"]["name"].as_str() {
                    // The tool's own answer, not a refusal: a Personal Agent
                    // hands it on as the Company wrote it.
                    Some("search") if args["query"] == "coupon" => ok(json!({
                        "isError": true,
                        "content": [{"type": "text", "text": format!(
                            "coupons are not sold here: {} and {} are the tool's words, not a refusal",
                            err::SIGN_IN_REQUIRED, err::EXTENSION_REQUIRED
                        )}]
                    })),
                    Some("search") => ok(json!({
                        "content": [{"type": "text", "text": "1 result"}],
                        "structuredContent": {"results": [{"sku": "stormline-insulated-m", "query": args["query"]}]}
                    })),
                    Some("order_status") if !signed_in => {
                        challenge(403, Some(err::SIGN_IN_REQUIRED))
                    }
                    Some("order_status") if !scope.split(' ').any(|s| s == wire::SCOPE_READ) => {
                        Reply::new(403)
                            .with(
                                "WWW-Authenticate",
                                &format!(
                                    "Bearer error=\"{}\", scope=\"{}\"",
                                    err::INSUFFICIENT_SCOPE,
                                    wire::SCOPE_READ
                                ),
                            )
                            .note("error", json!(err::INSUFFICIENT_SCOPE))
                    }
                    Some("order_status") => ok(json!({
                        "content": [{"type": "text", "text": "shipped"}],
                        "structuredContent": {"order_id": args["order_id"], "status": "shipped"}
                    })),
                    _ => rpc_err(-32602, "unknown tool"),
                };
                answer.note("idempotency_key", key)
            }
            _ => rpc_err(-32601, "method not found"),
        };
        let reply = reply
            .note("mcp", json!(method))
            .note("meta", json!(meta))
            .note("trace_id", json!(trace_id))
            .note("client_title", json!(title))
            .note("mcp_session", json!(mcp_session));
        if stalled {
            reply.note("stalled", json!(true))
        } else {
            reply
        }
    }

    // --- Steering the fake ---------------------------------------------

    fn control(&self, req: &Request) -> Reply {
        let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
            return Reply::new(400);
        };
        let mut st = lock(&self.state);
        let a = &body["approver"];
        match a["decision"].as_str() {
            Some("approve") => st.approver.decision = Decision::Approve,
            Some("deny") => st.approver.decision = Decision::Deny,
            Some("expire") => st.approver.decision = Decision::Expire,
            Some(_) => return Reply::new(400),
            None => {}
        }
        if let Some(account) = a["account"].as_str() {
            st.approver.account = account.to_string();
        }
        if let Some(n) = a["after_polls"].as_u64() {
            st.approver.after_polls = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(scope) = a["scope"].as_str() {
            st.approver.scope = Some(scope.split(' ').map(str::to_string).collect());
        }
        if let Some(n) = body["expires_in"].as_u64().filter(|n| *n > 0) {
            st.expires_in = n;
        }
        if let Some(n) = body["refuse_tokens"].as_u64() {
            st.refuse_tokens = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(n) = body["refuse_grants"].as_u64() {
            st.refuse_grants = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(ext) = body["extension_required"].as_str() {
            st.extension_required = Some(ext.to_string());
        }
        if body["extra_mcp"] == true {
            self.extra_mcp.store(true, Ordering::SeqCst);
        }
        if body["end_sessions"] == true {
            st.sessions.clear();
        }
        for (key, fails) in [("stall", false), ("fail", true)] {
            let h = &body[key];
            let Some(method) = h["method"].as_str() else {
                continue;
            };
            let status = match h["status"].as_u64().and_then(|s| u16::try_from(s).ok()) {
                Some(s) => Some(s),
                None if fails => return Reply::new(400),
                None => None,
            };
            let body = ["error", "result"]
                .into_iter()
                .find_map(|k| h.get(k).map(|v| json!({k: v})));
            st.holds.push(Hold {
                method: method.to_string(),
                times: h["times"]
                    .as_u64()
                    .map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX)),
                ms: h["ms"].as_u64().unwrap_or(0),
                status,
                body,
            });
        }
        if let Some(n) = body["date_skew_s"].as_i64() {
            st.date_skew_s = n;
        }
        Reply::json(200, &json!({"ok": true}))
    }
}

/// `unix_s` as an IMF-fixdate (RFC 9110 §5.6.7), the form a `Date` takes.
fn imf_date(unix_s: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (days, secs) = (unix_s.div_euclid(86_400), unix_s.rem_euclid(86_400));
    // Days to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// The OAuth error a refused proof is (§4.2's table).
fn proof_refusal(e: ProofError) -> Refusal {
    match e {
        ProofError::Invalid(why) => refuse(400, err::INVALID_DPOP_PROOF, why),
        ProofError::Nonce(n) => Refusal {
            nonce: Some(n),
            ..refuse(400, err::USE_DPOP_NONCE, "a nonce is required")
        },
    }
}

/// How the record names a token it was sent: the first eight characters of
/// its SHA-256, base64url-encoded, enough to tell tokens apart and never the
/// token itself.
fn presented(token: &str) -> String {
    let mut h = b64::url_nopad(&crate::sha::sha256(token.as_bytes()));
    h.truncate(8);
    h
}

/// A device sign-in's user code: eight letters a person can read and type,
/// `WDJB-MJHT` in the draft's example.
fn user_code() -> String {
    const LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
    let mut raw = [0u8; 8];
    let _ = crate::sec::random::fill(&mut raw);
    let letters: String = raw
        .iter()
        .map(|b| LETTERS[usize::from(*b) % LETTERS.len()] as char)
        .collect();
    format!("{}-{}", &letters[..4], &letters[4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reply's `Date` is an IMF-fixdate, which agentd reads back as
    /// the same instant.
    #[test]
    fn a_reply_is_dated_in_the_form_http_takes() {
        assert_eq!(imf_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(imf_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        let at = 1_792_368_000 + 3_723;
        assert_eq!(crate::net::http::http_date(&imf_date(at), at), Some(at));
    }

    fn fake(variants: &[Variant]) -> Company {
        Company::new(8443, variants.to_vec(), None)
    }

    fn get(path: &str) -> Request {
        Request {
            method: "GET".into(),
            path: path.into(),
            query: String::new(),
            headers: vec![("host".into(), "127.0.0.1:8443".into())],
            body: Vec::new(),
        }
    }

    fn body(r: &Reply) -> Value {
        serde_json::from_slice(&r.body).unwrap()
    }

    /// What the fake publishes holds to the kit's schemas: the discovery
    /// document, its metadata, the MCP server's metadata, and an error.
    #[test]
    fn its_documents_hold_to_the_kit() {
        let c = fake(&[Variant::TwoMcpApis]);
        let k = kit::kit();
        let d = c.route(&get(wire::DISCOVERY_PATH));
        assert_eq!(d.status, 200);
        let doc = body(&d);
        k.validate("poppy.schema.json", &doc).unwrap();
        assert_eq!(doc["organization"]["domain"], "127.0.0.1");
        assert_eq!(doc["apis"].as_array().unwrap().len(), 3);
        let path = format!("/.well-known/{}/auth", wire::AS_METADATA_SUFFIX);
        let m = body(&c.route(&get(&path)));
        k.validate("authorization-server-metadata.schema.json", &m)
            .unwrap();
        assert_eq!(m["issuer"], doc["auth"]["issuer"]);
        for mcp in ["/mcp", "/mcp/orders"] {
            let path = format!("/.well-known/{}{mcp}", wire::RESOURCE_METADATA_SUFFIX);
            let prm = body(&c.route(&get(&path)));
            k.validate("protected-resource-metadata.schema.json", &prm)
                .unwrap();
            assert_eq!(prm["resource"], format!("https://127.0.0.1:8443{mcp}"));
            assert_eq!(prm["authorization_servers"][0], doc["auth"]["issuer"]);
        }
        let e = c.oauth_error(refuse(400, err::INVALID_GRANT, "x"));
        k.validate("oauth-error.schema.json", &body(&e)).unwrap();
        let unchallenged = c.route(&Request {
            method: "POST".into(),
            ..get("/mcp")
        });
        assert_eq!(unchallenged.status, 401);
        assert!(
            unchallenged
                .headers
                .iter()
                .any(|(k, v)| k == "WWW-Authenticate" && v.contains("resource_metadata="))
        );
    }

    /// Each metadata flaw is the one its variant names, and nothing else.
    #[test]
    fn each_metadata_variant_has_exactly_its_flaw() {
        let path = format!("/.well-known/{}/auth", wire::AS_METADATA_SUFFIX);
        let good = body(&fake(&[]).route(&get(&path)));
        let differs = |v: Variant| {
            let m = body(&fake(&[v]).route(&get(&path)));
            let keys = |x: &Value| {
                x.as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>()
            };
            keys(&m)
                .union(&keys(&good))
                .filter(|k| m.get(*k) != good.get(*k))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(differs(Variant::IssuerMismatch), ["issuer"]);
        assert_eq!(differs(Variant::NoDomains), [wire::DOMAINS_MEMBER]);
        assert_eq!(differs(Variant::NoEs256), [wire::DPOP_ALGS_MEMBER]);
        assert_eq!(
            differs(Variant::PrefixedAlgs),
            [wire::DPOP_ALGS_MEMBER, wire::PREFIXED_DPOP_ALGS_MEMBER]
        );
        let off = differs(Variant::OffHostEndpoints);
        assert_eq!(off.len(), 4, "{off:?}");
        assert!(off.iter().all(|k| k.ends_with("_endpoint")), "{off:?}");
        let m = body(&fake(&[Variant::IssuerMismatch]).route(&get(&path)));
        assert_ne!(m["issuer"], good["issuer"]);
        let r = fake(&[Variant::RedirectToHttp]).route(&get(wire::DISCOVERY_PATH));
        assert_eq!(r.status, 301);
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "Location" && v.starts_with("http://"))
        );
        for v in Variant::ALL {
            assert_eq!(Variant::parse(v.name()), Some(v));
        }
    }

    /// A document that breaks its schema is never sent: the fake answers
    /// `500` naming the schema instead, patterns included, so it cannot
    /// drift from the kit unseen.
    #[test]
    fn a_document_that_breaks_its_schema_is_not_sent() {
        let c = fake(&[]);
        let bad = json!({"access_token": "", "token_type": "MAC", "expires_in": 0,
            "scope": "", "session_id": "s", "signed_in": false});
        let r = c.conformant("token-response.schema.json", bad).unwrap_err();
        assert_eq!(r.status, 500);
        assert!(String::from_utf8_lossy(&r.body).contains("token-response.schema.json"));
        let quoted = json!({"error": "invalid\"grant"});
        assert!(
            c.conformant("oauth-error.schema.json", quoted).is_err(),
            "an error code is held to its pattern"
        );
        let good = json!({"error": err::INVALID_GRANT});
        assert!(c.conformant("oauth-error.schema.json", good).is_ok());
    }

    /// A form decodes as RFC 6749 §3.1 reads one, and a user code is the
    /// draft's shape.
    #[test]
    fn forms_and_user_codes() {
        let f = decode_form(b"a=1&b=x%20y+z&c=").unwrap();
        assert_eq!(f.get("a").map(String::as_str), Some("1"));
        assert_eq!(f.get("b").map(String::as_str), Some("x y z"));
        assert!(!f.contains_key("c"), "an empty value is absent");
        assert!(decode_form(b"a=1&a=2").is_err());
        assert!(decode_form(b"a=%zz").is_err());
        let code = user_code();
        assert_eq!(code.len(), 9);
        assert_eq!(&code[4..5], "-");
    }
}
