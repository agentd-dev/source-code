// SPDX-License-Identifier: AGPL-3.0-only
//! A mock enterprise IdP, a mock resource authorization server, a mock
//! EMA MCP server and a mock A2A peer, for tests and the `ema` conformance
//! family. Hidden mode: `agentd --internal-mock-xaa <addr-file> [--knobs
//! <json>]`.
//!
//! Four loopback listeners, announced in `<addr-file>` as
//! `{"idp": …, "ras": …, "mcp": …, "mcp2": …, "peer": …}` (the MCP server's
//! URL with its `/mcp` path); each origin is its own issuer, so the IdP is
//! never the authorization server.
//!
//! - **The IdP** publishes its metadata split as Okta's org authorization
//!   server does — `jwks_uri` only in the OpenID document, the
//!   identity-chaining member only in the RFC 8414 one — and serves the
//!   device grant (approved at once), the authorization code (approved at
//!   once: `/authorize` redirects straight back), refresh, RFC 8693 token
//!   exchange for an ID-JAG, and RFC 7009 revocation. Its ID tokens and
//!   ID-JAGs are ES256, signed with a key it makes at start. Refresh tokens
//!   belong to families: with `rotate`, every use issues a new one and a
//!   used one presented again revokes its family, as reuse detection does.
//!   As the MCP server's own authorization server (RFC 0044 M1b) it also
//!   exchanges an access token it issued — from a refresh, or one it
//!   signed — for one the MCP server takes (RFC 8693, `requested_token_type`
//!   access_token), and it signs any token a test asks for
//!   (`POST <idp>/_mock/mint`, `{header, claims}`): the access tokens an
//!   A2A caller presents.
//! - **The authorization server** takes the JWT bearer grant: it verifies
//!   the ID-JAG with the IdP's key and holds it to `typ`, `aud`, `exp` and
//!   `client_id`, then issues an access token for the ID-JAG's `resource`.
//! - **The MCP server** (2025-11-25, `initialize`) takes only a token the
//!   authorization server issued for its URL, serves `list_issues`,
//!   `create_issue` and `whoami`, and records what each request declared. It
//!   answers at `/mcp` and at `/mcp2`, two resources behind one
//!   authorization server.
//! - **The A2A peer** (JSON-RPC, no streaming) answers every `SendMessage`
//!   with a task, completed at once — or, with `peer_auth`, in
//!   `TASK_STATE_AUTH_REQUIRED` saying where to sign in, until the test sets
//!   `peer_authorized`.
//!
//! Every request it receives is recorded (`GET <idp>/_mock/record`), with
//! its form or JSON-RPC body and the headers that matter, and a test steers
//! it with `POST <idp>/_mock/control` — a JSON object merged into its knobs
//! ([`Knobs`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::auth::client_assertion::ClientKey;
use crate::auth::idjag::wire;
use crate::b64;

/// What a test can change, and the defaults.
pub fn default_knobs() -> Value {
    json!({
        "sub": "00u1alice",
        "email": "alice@acme.example",
        "idp_client_id": "agentd-idp",
        "ras_client_id": "agentd-at-ras",
        // Refresh tokens rotate on every use, with reuse detection.
        "rotate": false,
        // The exchange answers with a refresh token too.
        "exchange_refresh_token": false,
        // The IdP answers 503 to everything but its record.
        "idp_down": false,
        // Every refresh token is refused: the person was deactivated.
        "rt_dead": false,
        // The exchange answers this OAuth error instead — for every
        // resource, or only the one `exchange_error_resource` names.
        "exchange_error": null,
        "exchange_error_resource": null,
        // The exchange wants a sign-in this recent (seconds).
        "step_up_max_age": null,
        // Claims merged into every ID-JAG; a null removes one. With
        // `id_jag_resource`, they and `id_jag_typ` apply only to an exchange
        // for that resource.
        "id_jag": {},
        "id_jag_typ": wire::TYP_ID_JAG,
        "id_jag_resource": null,
        // The scope the exchange grants, when not the one asked.
        "narrow_scope": null,
        // How long the IdP's token endpoint takes.
        "slow_ms": 0,
        // The ID token's sub, when not `sub`.
        "id_token_sub": null,
        // The authorization server refuses every ID-JAG.
        "ras_untrusted": false,
        // …or each one presented a second time.
        "ras_single_use": false,
        // Its metadata leaves out the ID-JAG profile.
        "ras_omit_profile": false,
        "at_expires_in": 3600,
        // Access tokens are JWTs for this audience (unset: opaque).
        "at_jwt_aud": null,
        // The MCP server refuses every token, or the next one.
        "mcp_reject": false,
        "mcp_reject_once": false,
        // Its tools/call answers a JSON-RPC error quoting the token.
        "mcp_error_echo": false,
        // Its protected-resource metadata names this resource, or these
        // authorization servers, instead of its own.
        "prm_resource": null,
        "prm_authorization_servers": null,
        // A tools/call of `{tool}` first asks the client `{message}` in the
        // shape `{schema}` (`elicitation/create`, over the call's own event
        // stream), and answers with what the client said.
        "elicit": null,
        // The access-token exchange (M1b) issues its token for this
        // audience instead of the one asked.
        "m1b_aud": null,
        // The peer's tasks wait on a sign-in, until `peer_authorized`.
        "peer_auth": false,
        "peer_authorized": false,
    })
}

/// A refresh token: whose, and which family it belongs to.
#[derive(Clone)]
struct Rt {
    sub: String,
    family: u64,
    used: bool,
    auth_time: u64,
}

/// An access token the authorization server issued.
#[derive(Clone)]
struct At {
    sub: String,
    resource: String,
    scope: String,
    exp: u64,
}

struct State {
    knobs: Value,
    key: ClientKey,
    idp: String,
    ras: String,
    mcp: String,
    rts: BTreeMap<String, Rt>,
    revoked_families: BTreeSet<u64>,
    families: u64,
    codes: BTreeMap<String, (String, Option<String>)>,
    jtis: BTreeSet<String>,
    ats: BTreeMap<String, At>,
    /// The access tokens the IdP issued to agentd, by whose they are.
    idp_ats: BTreeMap<String, String>,
    /// The peer's tasks.
    peer_tasks: BTreeSet<String>,
    peer: String,
    record: Vec<Value>,
    serial: u64,
    /// The client's answer to the last elicitation.
    elicited: Option<Value>,
}

impl State {
    fn knob(&self, k: &str) -> &Value {
        &self.knobs[k]
    }
    fn flag(&self, k: &str) -> bool {
        self.knobs[k].as_bool().unwrap_or(false)
    }
    fn next(&mut self, prefix: &str) -> String {
        self.serial += 1;
        format!(
            "{prefix}-{}-{}",
            self.serial,
            crate::sec::random::hex_token(6).unwrap_or_default()
        )
    }
    fn new_rt(&mut self, sub: &str, family: Option<u64>, auth_time: u64) -> String {
        let family = family.unwrap_or_else(|| {
            self.families += 1;
            self.families
        });
        let rt = self.next("rt");
        self.rts.insert(
            rt.clone(),
            Rt {
                sub: sub.to_string(),
                family,
                used: false,
                auth_time,
            },
        );
        rt
    }
}

fn now() -> u64 {
    crate::auth::cache::now_ms() / 1000
}

/// The running mock.
pub struct Mock {
    pub idp: String,
    pub ras: String,
    pub mcp: String,
    /// The second resource, `/mcp2`.
    pub mcp2: String,
    /// The A2A peer's URL.
    pub peer: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    /// Merge `knobs` into the mock's.
    pub fn control(&self, knobs: &Value) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        merge(&mut s.knobs, knobs);
    }

    /// Every request received so far.
    pub fn record(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record
            .clone()
    }

    /// The mock's public signing key, as a key set.
    pub fn jwks(&self) -> Value {
        json!({"keys": [self.state.lock().unwrap_or_else(|e| e.into_inner()).key.public_jwk()]})
    }
}

fn merge(into: &mut Value, from: &Value) {
    if let (Some(a), Some(b)) = (into.as_object_mut(), from.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
}

/// Start the mock in this process, with `knobs` over the defaults.
pub fn start(knobs: &Value) -> std::io::Result<Mock> {
    let bind = || TcpListener::bind("127.0.0.1:0");
    let (li, lr, lm, lp) = (bind()?, bind()?, bind()?, bind()?);
    let origin =
        |l: &TcpListener| -> std::io::Result<String> { Ok(format!("http://{}", l.local_addr()?)) };
    let (idp, ras, mcp_origin) = (origin(&li)?, origin(&lr)?, origin(&lm)?);
    let peer = origin(&lp)?;
    let mcp = format!("{mcp_origin}/mcp");
    let key = ClientKey::from_pkcs8(&ClientKey::generate_pkcs8().map_err(std::io::Error::other)?)
        .map_err(std::io::Error::other)?
        .with_kid("mock-idp-1");
    let mut k = default_knobs();
    merge(&mut k, knobs);
    let state = Arc::new(Mutex::new(State {
        knobs: k,
        key,
        idp: idp.clone(),
        ras: ras.clone(),
        mcp: mcp.clone(),
        rts: BTreeMap::new(),
        revoked_families: BTreeSet::new(),
        families: 0,
        codes: BTreeMap::new(),
        jtis: BTreeSet::new(),
        ats: BTreeMap::new(),
        idp_ats: BTreeMap::new(),
        peer_tasks: BTreeSet::new(),
        peer: peer.clone(),
        record: Vec::new(),
        serial: 0,
        elicited: None,
    }));
    for (l, role) in [
        (li, Role::Idp),
        (lr, Role::Ras),
        (lm, Role::Mcp),
        (lp, Role::Peer),
    ] {
        let state = Arc::clone(&state);
        std::thread::Builder::new()
            .name(format!("mock-xaa-{role:?}"))
            .spawn(move || {
                for c in l.incoming().flatten() {
                    let state = Arc::clone(&state);
                    std::thread::spawn(move || serve(c, role, &state));
                }
            })?;
    }
    Ok(Mock {
        idp,
        ras,
        mcp2: format!("{mcp_origin}/mcp2"),
        mcp,
        peer,
        state,
    })
}

/// `--internal-mock-xaa`: start, announce, serve until killed.
pub fn run_with(addr_file: &str, knobs: &Value) -> i32 {
    let mock = match start(knobs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mock-xaa: {e}");
            return crate::exit::USAGE;
        }
    };
    let doc = json!({"idp": mock.idp, "ras": mock.ras, "mcp": mock.mcp, "mcp2": mock.mcp2,
                     "peer": mock.peer});
    let tmp = format!("{addr_file}.tmp");
    let announced =
        std::fs::write(&tmp, doc.to_string()).and_then(|()| std::fs::rename(&tmp, addr_file));
    if let Err(e) = announced {
        eprintln!("mock-xaa: announce: {e}");
        return crate::exit::USAGE;
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Idp,
    Ras,
    Mcp,
    Peer,
}

/// One HTTP request.
struct Http {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Http {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    }
    fn form(&self) -> BTreeMap<String, String> {
        String::from_utf8_lossy(&self.body)
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), crate::auth::browser::pct_decode(v)))
            .collect()
    }
}

fn read(stream: &TcpStream) -> Option<Http> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    let (path, q) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), crate::auth::browser::pct_decode(v)))
        .collect();
    Some(Http {
        method,
        path: path.to_string(),
        query,
        headers,
        body,
    })
}

fn write(mut stream: &TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_reply(stream: &TcpStream, status: u16, v: &Value) {
    write(
        stream,
        status,
        &[
            ("Content-Type", "application/json".into()),
            ("Cache-Control", "no-store".into()),
        ],
        v.to_string().as_bytes(),
    );
}

fn oauth_error(stream: &TcpStream, status: u16, code: &str, extra: Value) {
    let mut v = json!({"error": code});
    merge(&mut v, &extra);
    json_reply(stream, status, &v);
}

fn serve(stream: TcpStream, role: Role, state: &Arc<Mutex<State>>) {
    let Some(req) = read(&stream) else { return };
    // The record and the knobs, on the IdP's listener.
    if matches!(role, Role::Idp) && req.path.starts_with("/_mock/") {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        match req.path.as_str() {
            "/_mock/record" => {
                let v = Value::Array(s.record.clone());
                drop(s);
                json_reply(&stream, 200, &v);
            }
            "/_mock/control" => {
                let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                merge(&mut s.knobs, &v);
                drop(s);
                json_reply(&stream, 200, &json!({"ok": true}));
            }
            "/_mock/clear" => {
                s.record.clear();
                drop(s);
                json_reply(&stream, 200, &json!({"ok": true}));
            }
            // A token signed with the IdP's key: the header's members (a
            // null drops one; `alg` and `kid` are the key's) over the claims.
            "/_mock/mint" => {
                let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let mut header = json!({});
                if let Some(h) = v["header"].as_object() {
                    for (k, x) in h.iter().filter(|(_, x)| !x.is_null()) {
                        header[k] = x.clone();
                    }
                }
                let token = s.key.sign_jwt(header, &v["claims"]).unwrap_or_default();
                drop(s);
                json_reply(&stream, 200, &json!({"token": token}));
            }
            _ => write(&stream, 404, &[], b""),
        }
        return;
    }
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let body = match serde_json::from_slice::<Value>(&req.body) {
            Ok(v) => v,
            Err(_) => Value::Object(
                req.form()
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            ),
        };
        let pick = |k: &str| req.header(k).map(str::to_string);
        s.record.push(json!({
            "at": format!("{role:?}").to_lowercase(), "method": req.method, "path": req.path,
            "body": body, "authorization": pick("authorization"),
            "mcp_protocol_version": pick("mcp-protocol-version"), "t": crate::auth::cache::now_ms(),
        }));
    }
    match role {
        Role::Idp => idp(&stream, &req, state),
        Role::Ras => ras(&stream, &req, state),
        Role::Mcp => mcp(&stream, &req, state),
        Role::Peer => peer(&stream, &req, state),
    }
}

/// The client a token request authenticated, or why none: a JWT client
/// assertion (`iss` = `sub` = the client, `aud` the token endpoint or the
/// issuer) or `Basic` — a public client is refused.
fn client_of(req: &Http, token_endpoint: &str, issuer: &str) -> Result<String, String> {
    let form = req.form();
    if let Some(a) = form.get("client_assertion") {
        if form.get("client_assertion_type").map(String::as_str)
            != Some(crate::auth::client_assertion::ASSERTION_TYPE)
        {
            return Err("client_assertion_type".into());
        }
        let (_, c) = crate::auth::idjag::decode_unverified(a)?;
        let iss = c["iss"].as_str().unwrap_or_default();
        if c["sub"].as_str() != Some(iss) {
            return Err("assertion sub is not iss".into());
        }
        let aud = c["aud"].as_str().unwrap_or_default();
        if aud != token_endpoint && aud != issuer {
            return Err(format!("assertion aud {aud}"));
        }
        if c["exp"].as_u64().unwrap_or(0) <= now() {
            return Err("assertion expired".into());
        }
        return Ok(iss.to_string());
    }
    if let Some(basic) = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Basic "))
    {
        let pair = b64::std_decode(basic.trim()).map_err(|e| e.to_string())?;
        let pair = String::from_utf8_lossy(&pair).to_string();
        let (id, secret) = pair.split_once(':').ok_or("basic")?;
        if secret.is_empty() {
            return Err("an empty secret".into());
        }
        return Ok(crate::auth::browser::pct_decode(id));
    }
    if form.contains_key("client_secret") {
        return Ok(form.get("client_id").cloned().unwrap_or_default());
    }
    Err("no client authentication: a public client".into())
}

fn sign(s: &State, typ: &str, claims: &Value) -> String {
    s.key
        .sign_jwt(json!({"typ": typ}), claims)
        .unwrap_or_default()
}

fn id_token(s: &State, sub: &str, nonce: Option<&str>, auth_time: u64) -> String {
    let t = now();
    let mut c = json!({"iss": s.idp, "aud": s.knob("idp_client_id"), "sub": sub,
                       "iat": t, "exp": t + 3600, "auth_time": auth_time,
                       "email": s.knob("email")});
    if let Some(n) = nonce {
        c["nonce"] = json!(n);
    }
    sign(s, "JWT", &c)
}

fn idp(stream: &TcpStream, req: &Http, state: &Arc<Mutex<State>>) {
    let (down, slow) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        (s.flag("idp_down"), s.knob("slow_ms").as_u64().unwrap_or(0))
    };
    if down {
        return write(stream, 503, &[], b"down");
    }
    if req.path == "/token" && slow > 0 {
        std::thread::sleep(std::time::Duration::from_millis(slow));
    }
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let iss = s.idp.clone();
    let token_endpoint = format!("{iss}/token");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server") => {
            let v = json!({"issuer": iss, "token_endpoint": token_endpoint,
                "device_authorization_endpoint": format!("{iss}/device"),
                "authorization_endpoint": format!("{iss}/authorize"),
                "revocation_endpoint": format!("{iss}/revoke"),
                "grant_types_supported": ["authorization_code", "refresh_token",
                    "urn:ietf:params:oauth:grant-type:device_code", wire::GRANT_TOKEN_EXCHANGE],
                "code_challenge_methods_supported": ["S256"],
                wire::METADATA_IDENTITY_CHAINING: [wire::TOKEN_TYPE_ID_JAG]});
            drop(s);
            json_reply(stream, 200, &v);
        }
        ("GET", "/.well-known/openid-configuration") => {
            let v = json!({"issuer": iss, "token_endpoint": token_endpoint,
                "authorization_endpoint": format!("{iss}/authorize"),
                "jwks_uri": format!("{iss}/keys"),
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true});
            drop(s);
            json_reply(stream, 200, &v);
        }
        ("GET", "/keys") => {
            let v = json!({"keys": [s.key.public_jwk()]});
            drop(s);
            json_reply(stream, 200, &v);
        }
        ("POST", "/device") => {
            let code = s.next("dc");
            let sub = s.knob("sub").as_str().unwrap_or_default().to_string();
            s.codes.insert(code.clone(), (sub, None));
            drop(s);
            json_reply(
                stream,
                200,
                &json!({"device_code": code, "user_code": "MOCK-CODE", "verification_uri": format!("{iss}/activate"),
                        "verification_uri_complete": format!("{iss}/activate?user_code=MOCK-CODE"),
                        "expires_in": 600, "interval": 1}),
            );
        }
        ("GET", "/authorize") => {
            let q: BTreeMap<String, String> = req.query.iter().cloned().collect();
            let code = s.next("code");
            let sub = s.knob("sub").as_str().unwrap_or_default().to_string();
            s.codes.insert(code.clone(), (sub, q.get("nonce").cloned()));
            drop(s);
            let location = format!(
                "{}?code={}&state={}&iss={}",
                q.get("redirect_uri").cloned().unwrap_or_default(),
                code,
                crate::auth::browser::pct(q.get("state").map(String::as_str).unwrap_or_default()),
                crate::auth::browser::pct(&iss)
            );
            write(stream, 302, &[("Location", location)], b"");
        }
        ("POST", "/revoke") => {
            let form = req.form();
            if let Some(rt) = form.get("token").and_then(|t| s.rts.get(t)).cloned() {
                s.revoked_families.insert(rt.family);
            }
            drop(s);
            write(stream, 200, &[], b"");
        }
        ("POST", "/token") => {
            if let Err(e) = client_of(req, &token_endpoint, &iss) {
                drop(s);
                return oauth_error(
                    stream,
                    401,
                    "invalid_client",
                    json!({"error_description": e}),
                );
            }
            let form = req.form();
            let grant = form.get("grant_type").cloned().unwrap_or_default();
            let id_sub =
                |s: &State, sub: &str| s.knob("id_token_sub").as_str().unwrap_or(sub).to_string();
            match grant.as_str() {
                "urn:ietf:params:oauth:grant-type:device_code" | "authorization_code" => {
                    let key = form
                        .get(if grant == "authorization_code" {
                            "code"
                        } else {
                            "device_code"
                        })
                        .cloned()
                        .unwrap_or_default();
                    let Some((sub, nonce)) = s.codes.remove(&key) else {
                        drop(s);
                        return oauth_error(stream, 400, "invalid_grant", json!({}));
                    };
                    let t = now();
                    let rt = s.new_rt(&sub, None, t);
                    let it = id_token(&s, &id_sub(&s, &sub), nonce.as_deref(), t);
                    let at = s.next("idp-at");
                    s.idp_ats.insert(at.clone(), sub);
                    drop(s);
                    json_reply(
                        stream,
                        200,
                        &json!({"access_token": at, "token_type": "Bearer", "expires_in": 3600,
                                "refresh_token": rt, "id_token": it, "scope": "openid offline_access"}),
                    );
                }
                "refresh_token" => {
                    let presented = form.get("refresh_token").cloned().unwrap_or_default();
                    match use_rt(&mut s, &presented) {
                        Err(()) => {
                            drop(s);
                            oauth_error(stream, 400, "invalid_grant", json!({}))
                        }
                        Ok((rt, next)) => {
                            let it = id_token(&s, &id_sub(&s, &rt.sub), None, rt.auth_time);
                            let at = s.next("idp-at");
                            s.idp_ats.insert(at.clone(), rt.sub.clone());
                            let mut v = json!({"access_token": at, "token_type": "Bearer",
                                               "expires_in": 3600, "id_token": it});
                            if let Some(n) = next {
                                v["refresh_token"] = json!(n);
                            }
                            drop(s);
                            json_reply(stream, 200, &v);
                        }
                    }
                }
                wire::GRANT_TOKEN_EXCHANGE => exchange(stream, &form, s),
                _ => {
                    drop(s);
                    oauth_error(stream, 400, "unsupported_grant_type", json!({}))
                }
            }
        }
        _ => {
            drop(s);
            write(stream, 404, &[], b"");
        }
    }
}

/// Present refresh token `rt`: refused when unknown, dead, revoked or
/// spent (which revokes its family); with `rotate`, spent now and a new one
/// issued.
#[allow(clippy::result_unit_err)]
fn use_rt(s: &mut State, rt: &str) -> Result<(Rt, Option<String>), ()> {
    let Some(r) = s.rts.get(rt).cloned() else {
        return Err(());
    };
    if s.flag("rt_dead") || s.revoked_families.contains(&r.family) {
        return Err(());
    }
    if r.used {
        // Reuse detection: the whole family goes.
        s.revoked_families.insert(r.family);
        return Err(());
    }
    if !s.flag("rotate") {
        return Ok((r, None));
    }
    if let Some(x) = s.rts.get_mut(rt) {
        x.used = true;
    }
    let next = s.new_rt(&r.sub.clone(), Some(r.family), r.auth_time);
    Ok((r, Some(next)))
}

fn exchange(
    stream: &TcpStream,
    form: &BTreeMap<String, String>,
    mut s: std::sync::MutexGuard<'_, State>,
) {
    let get = |k: &str| form.get(k).cloned().unwrap_or_default();
    if get("requested_token_type") == wire::TOKEN_TYPE_ACCESS_TOKEN {
        return access_exchange(stream, form, s);
    }
    if get("requested_token_type") != wire::TOKEN_TYPE_ID_JAG || get("audience").is_empty() {
        drop(s);
        return oauth_error(stream, 400, "invalid_request", json!({}));
    }
    let errs_here = s
        .knob("exchange_error_resource")
        .as_str()
        .is_none_or(|r| r == get("resource"));
    if let Some(e) = s
        .knob("exchange_error")
        .as_object()
        .cloned()
        .filter(|_| errs_here)
    {
        drop(s);
        return oauth_error(
            stream,
            400,
            e.get("error")
                .and_then(Value::as_str)
                .unwrap_or("invalid_grant"),
            json!({}),
        );
    }
    // The subject: a refresh token, or an ID token this IdP signed.
    let (sub, auth_time, rotated) = match get("subject_token_type").as_str() {
        wire::TOKEN_TYPE_REFRESH_TOKEN => match use_rt(&mut s, &get("subject_token")) {
            Ok((rt, next)) => (rt.sub, rt.auth_time, next),
            Err(()) => {
                drop(s);
                return oauth_error(stream, 400, "invalid_grant", json!({}));
            }
        },
        wire::TOKEN_TYPE_ID_TOKEN => {
            let jwks = json!({"keys": [s.key.public_jwk()]});
            let aud = s
                .knob("idp_client_id")
                .as_str()
                .unwrap_or_default()
                .to_string();
            let iss = s.idp.clone();
            match crate::auth::idjag::verify_id_token(
                &get("subject_token"),
                &jwks,
                &iss,
                &aud,
                None,
                now(),
            ) {
                Ok(c) => (c.sub, c.auth_time.unwrap_or(0), None),
                Err(_) => {
                    drop(s);
                    return oauth_error(stream, 400, "invalid_grant", json!({}));
                }
            }
        }
        _ => {
            drop(s);
            return oauth_error(stream, 400, "invalid_request", json!({}));
        }
    };
    if let Some(m) = s.knob("step_up_max_age").as_u64()
        && auth_time + m < now()
    {
        drop(s);
        return oauth_error(
            stream,
            400,
            wire::INSUFFICIENT_USER_AUTHENTICATION,
            json!({"max_age": m}),
        );
    }
    let t = now();
    let scope = s
        .knob("narrow_scope")
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| get("scope"));
    let jti = s.next("jti");
    let mut claims = json!({"iss": s.idp, "sub": sub, "aud": get("audience"),
        "client_id": s.knob("ras_client_id"), "jti": jti, "iat": t, "exp": t + 300,
        "resource": get("resource"), "scope": scope, "auth_time": auth_time});
    let jag_here = s
        .knob("id_jag_resource")
        .as_str()
        .is_none_or(|r| r == get("resource"));
    if let (Some(c), Some(o), true) = (
        claims.as_object_mut(),
        s.knob("id_jag").as_object(),
        jag_here,
    ) {
        for (k, v) in o {
            if v.is_null() {
                c.remove(k);
            } else {
                c.insert(k.clone(), v.clone());
            }
        }
    }
    let typ = s
        .knob("id_jag_typ")
        .as_str()
        .filter(|_| jag_here)
        .unwrap_or(wire::TYP_ID_JAG)
        .to_string();
    let jag = sign(&s, &typ, &claims);
    let mut v = json!({"issued_token_type": wire::TOKEN_TYPE_ID_JAG, "access_token": jag,
                       "token_type": wire::TOKEN_TYPE_NA, "expires_in": 300, "scope": scope});
    if s.flag("exchange_refresh_token") {
        v["refresh_token"] = json!(rotated.clone().unwrap_or_else(|| "rt-from-exchange".into()));
    } else if let Some(r) = rotated {
        v["refresh_token"] = json!(r);
    }
    drop(s);
    json_reply(stream, 200, &v);
}

/// The claims of `jwt` when this IdP signed it, it names this IdP and it
/// has not expired.
fn own_claims(s: &State, jwt: &str) -> Option<Value> {
    let (input, sig) = jwt.rsplit_once('.')?;
    let jwk = s.key.public_jwk();
    let mut point = vec![0x04];
    point.extend(b64::url_decode(jwk["x"].as_str()?).ok()?);
    point.extend(b64::url_decode(jwk["y"].as_str()?).ok()?);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(input.as_bytes(), &b64::url_decode(sig).ok()?)
        .ok()?;
    let (_, c) = crate::auth::idjag::decode_unverified(jwt).ok()?;
    (c["iss"].as_str() == Some(s.idp.as_str()) && c["exp"].as_u64().unwrap_or(0) > now())
        .then_some(c)
}

/// The on-behalf-of exchange (RFC 8693, M1b): an access token this IdP
/// issued for agentd — from a refresh, or a JWT it signed (an A2A caller's
/// token) — for one the MCP server takes, its `aud` the one asked (or
/// `m1b_aud`), whose it is in `sub` and `uid`.
fn access_exchange(
    stream: &TcpStream,
    form: &BTreeMap<String, String>,
    mut s: std::sync::MutexGuard<'_, State>,
) {
    let get = |k: &str| form.get(k).cloned().unwrap_or_default();
    if get("subject_token_type") != wire::TOKEN_TYPE_ACCESS_TOKEN || get("audience").is_empty() {
        drop(s);
        return oauth_error(stream, 400, "invalid_request", json!({}));
    }
    if let Some(e) = s.knob("exchange_error").as_object().cloned() {
        drop(s);
        return oauth_error(
            stream,
            400,
            e.get("error")
                .and_then(Value::as_str)
                .unwrap_or("invalid_grant"),
            json!({}),
        );
    }
    let subject = get("subject_token");
    let sub = s.idp_ats.get(&subject).cloned().or_else(|| {
        own_claims(&s, &subject).and_then(|c| {
            c["uid"]
                .as_str()
                .or_else(|| c["sub"].as_str())
                .map(str::to_string)
        })
    });
    let Some(sub) = sub else {
        drop(s);
        return oauth_error(stream, 400, "invalid_grant", json!({}));
    };
    let t = now();
    let scope = s
        .knob("narrow_scope")
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| get("scope"));
    let aud = s
        .knob("m1b_aud")
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| get("audience"));
    let jti = s.next("m1b");
    let scp: Vec<&str> = scope.split_whitespace().collect();
    let at = sign(
        &s,
        "at+jwt",
        &json!({"iss": s.idp, "aud": aud, "sub": sub, "uid": sub, "cid": get("client_id"),
                "scp": scp, "jti": jti, "iat": t, "exp": t + 300}),
    );
    s.ats.insert(
        at.clone(),
        At {
            sub,
            resource: get("resource"),
            scope: scope.clone(),
            exp: t + 300,
        },
    );
    let mut v = json!({"issued_token_type": wire::TOKEN_TYPE_ACCESS_TOKEN, "access_token": at,
                       "token_type": "Bearer", "expires_in": 300, "scope": scope});
    if s.flag("exchange_refresh_token") {
        v["refresh_token"] = json!("rt-from-exchange");
    }
    drop(s);
    json_reply(stream, 200, &v);
}

/// The A2A peer: its card, and `SendMessage` and `GetTask` over JSON-RPC.
fn peer(stream: &TcpStream, req: &Http, state: &Arc<Mutex<State>>) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    if req.method == "GET" && req.path == "/.well-known/agent-card.json" {
        let v = json!({"name": "mock-peer", "description": "a mock A2A peer", "version": "1",
            "supportedInterfaces": [{"url": s.peer, "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
            "capabilities": {"streaming": false},
            "defaultInputModes": ["text/plain"], "defaultOutputModes": ["text/plain"], "skills": []});
        drop(s);
        return write(
            stream,
            200,
            &[
                ("Content-Type", "application/json".into()),
                ("Cache-Control", "no-store".into()),
            ],
            v.to_string().as_bytes(),
        );
    }
    if req.method != "POST" {
        drop(s);
        return write(stream, 404, &[], b"");
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body["method"].as_str().unwrap_or_default();
    let task = match method {
        "SendMessage" => {
            let t = s.next("pt");
            s.peer_tasks.insert(t.clone());
            t
        }
        "GetTask" => body["params"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        _ => {
            drop(s);
            return json_reply(
                stream,
                200,
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
            );
        }
    };
    if !s.peer_tasks.contains(&task) {
        drop(s);
        return json_reply(
            stream,
            200,
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": "Task not found"}}),
        );
    }
    let waits = s.flag("peer_auth") && !s.flag("peer_authorized");
    let mut t = json!({"id": task, "contextId": format!("ctx-{task}"),
                       "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "1970-01-01T00:00:00.000Z"},
                       "artifacts": [{"artifactId": format!("{task}.answer"), "parts": [{"text": "the peer's answer"}]}]});
    if waits {
        t = json!({"id": task, "contextId": format!("ctx-{task}"),
                   "status": {"state": "TASK_STATE_AUTH_REQUIRED", "timestamp": "1970-01-01T00:00:00.000Z",
                              "message": {"messageId": format!("{task}.ask"), "role": "ROLE_AGENT",
                                          "parts": [{"text": format!("Sign in at {}/activate so I may read your calendar", s.idp)}]}}});
    }
    drop(s);
    let result = if method == "SendMessage" {
        json!({"task": t})
    } else {
        t
    };
    json_reply(
        stream,
        200,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn ras(stream: &TcpStream, req: &Http, state: &Arc<Mutex<State>>) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let iss = s.ras.clone();
    let token_endpoint = format!("{iss}/token");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server") => {
            let mut v = json!({"issuer": iss, "token_endpoint": token_endpoint,
                "grant_types_supported": [wire::GRANT_JWT_BEARER, "client_credentials"],
                wire::METADATA_GRANT_PROFILES: [wire::PROFILE_ID_JAG]});
            if s.flag("ras_omit_profile") {
                v.as_object_mut()
                    .map(|o| o.remove(wire::METADATA_GRANT_PROFILES));
            }
            drop(s);
            json_reply(stream, 200, &v);
        }
        ("POST", "/token") => {
            let client = match client_of(req, &token_endpoint, &iss) {
                Ok(c) => c,
                Err(e) => {
                    drop(s);
                    return oauth_error(
                        stream,
                        401,
                        "invalid_client",
                        json!({"error_description": e}),
                    );
                }
            };
            let form = req.form();
            if form.get("grant_type").map(String::as_str) != Some(wire::GRANT_JWT_BEARER) {
                drop(s);
                return oauth_error(stream, 400, "unsupported_grant_type", json!({}));
            }
            let assertion = form.get("assertion").cloned().unwrap_or_default();
            let jwks = json!({"keys": [s.key.public_jwk()]});
            // The ID-JAG: the IdP's signature, then its claims.
            let ok = (|| -> Result<Value, String> {
                let (h, c) = crate::auth::idjag::decode_unverified(&assertion)?;
                if h["typ"].as_str() != Some(wire::TYP_ID_JAG) {
                    return Err("typ".into());
                }
                let iss_idp = s.idp.clone();
                let (input, sig) = assertion.rsplit_once('.').ok_or("jws")?;
                let jwk = &jwks["keys"][0];
                let mut point = vec![0x04];
                point.extend(b64::url_decode(jwk["x"].as_str().unwrap_or_default())?);
                point.extend(b64::url_decode(jwk["y"].as_str().unwrap_or_default())?);
                ring::signature::UnparsedPublicKey::new(
                    &ring::signature::ECDSA_P256_SHA256_FIXED,
                    &point,
                )
                .verify(input.as_bytes(), &b64::url_decode(sig)?)
                .map_err(|_| "signature".to_string())?;
                if c["iss"].as_str() != Some(iss_idp.as_str()) {
                    return Err("iss".into());
                }
                if c["aud"].as_str() != Some(iss.as_str()) {
                    return Err("aud".into());
                }
                if c["client_id"].as_str() != Some(client.as_str()) {
                    return Err("client_id".into());
                }
                if c["exp"].as_u64().unwrap_or(0) <= now() {
                    return Err("exp".into());
                }
                Ok(c)
            })();
            let claims = match ok {
                Ok(c) if !s.flag("ras_untrusted") => c,
                _ => {
                    drop(s);
                    return oauth_error(stream, 400, "invalid_grant", json!({}));
                }
            };
            let jti = claims["jti"].as_str().unwrap_or_default().to_string();
            // Every jti is remembered, so a server turned single-use refuses
            // one it took before.
            let first = s.jtis.insert(jti);
            if s.flag("ras_single_use") && !first {
                drop(s);
                return oauth_error(
                    stream,
                    400,
                    "invalid_grant",
                    json!({"error_description": "reused"}),
                );
            }
            let ttl = s.knob("at_expires_in").as_u64().unwrap_or(3600);
            let resource = claims["resource"].as_str().unwrap_or_default().to_string();
            let sub = claims["sub"].as_str().unwrap_or_default().to_string();
            let scope = claims["scope"].as_str().unwrap_or_default().to_string();
            let at = match s.knob("at_jwt_aud").as_str().map(str::to_string) {
                Some(aud) => {
                    let jti = s.next("at");
                    sign(
                        &s,
                        "at+jwt",
                        &json!({"iss": iss, "aud": aud, "sub": sub, "jti": jti, "exp": now() + ttl}),
                    )
                }
                None => s.next("at"),
            };
            s.ats.insert(
                at.clone(),
                At {
                    sub,
                    resource,
                    scope: scope.clone(),
                    exp: now() + ttl,
                },
            );
            drop(s);
            json_reply(
                stream,
                200,
                &json!({"access_token": at, "token_type": "Bearer", "expires_in": ttl, "scope": scope,
                        "refresh_token": "never-to-be-kept"}),
            );
        }
        _ => {
            drop(s);
            write(stream, 404, &[], b"");
        }
    }
}

fn rpc(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp(stream: &TcpStream, req: &Http, state: &Arc<Mutex<State>>) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let origin = s.mcp.trim_end_matches("/mcp").to_string();
    let path = req
        .path
        .strip_prefix("/.well-known/oauth-protected-resource")
        .unwrap_or(&req.path)
        .to_string();
    if !matches!(path.as_str(), "/mcp" | "/mcp2") {
        drop(s);
        return write(stream, 404, &[], b"");
    }
    let url = format!("{origin}{path}");
    let prm = format!("{origin}/.well-known/oauth-protected-resource{path}");
    if req.method == "GET" && req.path.starts_with("/.well-known/") {
        let resource = s.knob("prm_resource").as_str().unwrap_or(&url).to_string();
        let servers = match s.knob("prm_authorization_servers") {
            Value::Array(a) => Value::Array(a.clone()),
            _ => json!([s.ras]),
        };
        let v = json!({"resource": resource, "authorization_servers": servers});
        drop(s);
        return json_reply(stream, 200, &v);
    }
    if req.method != "POST" {
        drop(s);
        return write(stream, 405, &[], b"");
    }
    let token = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let reject_once = s.flag("mcp_reject_once");
    let at = s
        .ats
        .get(&token)
        .cloned()
        .filter(|a| a.resource == url && a.exp > now());
    let Some(at) = at.filter(|_| !s.flag("mcp_reject") && !reject_once) else {
        if reject_once {
            s.knobs["mcp_reject_once"] = json!(false);
        }
        drop(s);
        return write(
            stream,
            401,
            &[(
                "WWW-Authenticate",
                format!("Bearer error=\"invalid_token\", resource_metadata=\"{prm}\""),
            )],
            b"",
        );
    };
    let echo = s.flag("mcp_error_echo");
    let elicit = s.knob("elicit").clone();
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    // The client's answer to an elicitation this server sent.
    if body.get("method").is_none() && (body.get("result").is_some() || body.get("error").is_some())
    {
        s.elicited = Some(body);
        drop(s);
        return write(stream, 202, &[], b"");
    }
    drop(s);
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let m = body["method"].as_str().unwrap_or_default();
    if m == "tools/call" && body["params"]["name"] == elicit["tool"] {
        return ask_then_answer(stream, &id, &elicit, state);
    }
    let session = vec![
        ("Mcp-Session-Id", "s-ema".to_string()),
        ("Content-Type", "application/json".to_string()),
    ];
    let reply = |v: Value| write(stream, 200, &session, v.to_string().as_bytes());
    match m {
        "initialize" => reply(rpc(
            &id,
            json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {}},
                   "serverInfo": {"name": "mock-ema", "version": "1"}}),
        )),
        "server/discover" => reply(json!({"jsonrpc": "2.0", "id": id,
                                          "error": {"code": -32601, "message": "Method not found"}})),
        "tools/list" => reply(rpc(
            &id,
            json!({"tools": [
            {"name": "list_issues", "description": "List issues", "inputSchema": {"type": "object"}},
            {"name": "create_issue", "description": "Create an issue",
             "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]}},
            {"name": "whoami", "description": "Who the token is for", "inputSchema": {"type": "object"}}]}),
        )),
        "tools/call" if echo => reply(json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32000, "message": format!("the token Bearer {token} is not good here")}})),
        "tools/call" => {
            let name = body["params"]["name"].as_str().unwrap_or_default();
            let args = &body["params"]["arguments"];
            let out = match name {
                "create_issue" => {
                    json!({"id": "ISS-1", "title": args["title"], "by": at.sub, "scope": at.scope})
                }
                "list_issues" => {
                    json!({"issues": [{"id": "ISS-0", "title": "seeded"}], "for": at.sub})
                }
                "whoami" => json!({"sub": at.sub, "scope": at.scope, "echo": token}),
                _ => {
                    return reply(json!({"jsonrpc": "2.0", "id": id,
                                        "error": {"code": -32602, "message": "unknown tool"}}));
                }
            };
            reply(rpc(
                &id,
                json!({"content": [{"type": "text", "text": out.to_string()}], "structuredContent": out,
                       "isError": false}),
            ))
        }
        _ if m.starts_with("notifications/") => write(stream, 202, &[], b""),
        _ => reply(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}),
        ),
    }
}

/// A tools/call answered over an event stream: `elicitation/create` with
/// the knob's message and schema first, then — once the client answers, or
/// after 60 s — the call's result, which says what the client answered.
fn ask_then_answer(mut stream: &TcpStream, id: &Value, elicit: &Value, state: &Arc<Mutex<State>>) {
    state.lock().unwrap_or_else(|e| e.into_inner()).elicited = None;
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nMcp-Session-Id: s-ema\r\nConnection: close\r\n\r\n";
    let ask = json!({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create",
                     "params": {"message": elicit["message"], "requestedSchema": elicit["schema"]}});
    if stream
        .write_all(format!("{head}data: {ask}\n\n").as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let answered = loop {
        if let Some(a) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elicited
            .take()
        {
            break a;
        }
        if std::time::Instant::now() >= deadline {
            break Value::Null;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let out = json!({"answered": answered["result"]});
    let result = rpc(
        id,
        json!({"content": [{"type": "text", "text": out.to_string()}], "structuredContent": out,
               "isError": false}),
    );
    let _ = stream.write_all(format!("data: {result}\n\n").as_bytes());
    let _ = stream.flush();
}

/// A knob set as a `Map`, for a test that builds one up.
pub fn knobs(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<Map<String, Value>>(),
    )
}
