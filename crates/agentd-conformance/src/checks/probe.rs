// SPDX-License-Identifier: AGPL-3.0-only
//! A Personal Agent for the `companies` checks to drive the fake Company
//! with, written from the Personal Agent Protocol draft (spec 3, 4.1–4.3)
//! rather than from agentd, so the fake is held to the protocol and not to
//! agentd's reading of it.
//!
//! It holds two P-256 keys: the client key its key set publishes, which
//! signs client assertions and Session assertions (§4.1, §4.2), and a DPoP
//! key it publishes nowhere (§4.3). It serves its client metadata document
//! and key set over HTTPS on loopback, with the workspace's test PKI, and
//! sends HTTPS requests that trust the test CA and follow no redirect.
//! [`discover`] is a Personal Agent's discovery, every check of §3 and §3.2
//! applied, and the host rule agentd adds (rfcs/0047 §5.5.1).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

use crate::harness::{ConfServer, Harness, TempDir, tls_fixture};

/// The draft's constants the probe sends and reads, as the draft writes
/// them.
pub const DISCOVERY_PATH: &str = "/.well-known/poppy.json";
pub const DOMAINS: &str = "poppy_domains";
pub const PREFIXED_ALGS: &str = "poppy_dpop_signing_alg_values_supported";
pub const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
pub const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url (RFC 4648 §5).
pub fn b64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(B64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// The 128-bit random value a `jti` is.
pub fn random_id() -> String {
    let mut raw = [0u8; 16];
    ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut raw).expect("randomness");
    b64url(&raw)
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

/// A P-256 key, its public JWK and its RFC 7638 thumbprint.
pub struct Key {
    pair: EcdsaKeyPair,
    pub jwk: Value,
    pub kid: String,
}

impl Key {
    pub fn generate() -> Key {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("generate a P-256 key");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .expect("read it back");
        let point = pair.public_key().as_ref();
        let (x, y) = (b64url(&point[1..33]), b64url(&point[33..65]));
        // RFC 7638 §3.2: the required members, in order, no white space.
        let canon = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        Key {
            pair,
            jwk: json!({"kty": "EC", "crv": "P-256", "x": x, "y": y}),
            kid: b64url(&sha256(canon.as_bytes())),
        }
    }

    /// A compact JWS over `header` and `claims`, ES256.
    pub fn sign(&self, header: &Value, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let sig = self
            .pair
            .sign(&SystemRandom::new(), input.as_bytes())
            .expect("sign");
        format!("{input}.{}", b64url(sig.as_ref()))
    }
}

/// An HTTP answer.
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The OAuth error code a refusal carries, in its body or in its
    /// `WWW-Authenticate` challenge.
    pub fn error(&self) -> String {
        if let Some(e) = self.json()["error"].as_str() {
            return e.to_string();
        }
        self.header("www-authenticate")
            .and_then(|h| h.split("error=\"").nth(1))
            .and_then(|r| r.split('"').next())
            .unwrap_or_default()
            .to_string()
    }

    /// One line describing the answer, for a failure message.
    pub fn brief(&self) -> String {
        let body = String::from_utf8_lossy(&self.body);
        let challenge = self.header("www-authenticate").unwrap_or_default();
        format!(
            "HTTP {} {challenge} {}",
            self.status,
            &body[..body.len().min(300)]
        )
    }
}

/// One HTTPS request, trusting the test CA, following no redirect.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<Resp, String> {
    let u = net::http::Url::parse(url)?;
    if !u.is_tls() {
        return Err(format!("{url} is not an HTTPS URL"));
    }
    let ca = std::fs::read(tls_fixture("ca.pem")).map_err(|e| format!("the test CA: {e}"))?;
    let tcp = net::http::connect_tcp(&u.host, u.port, Duration::from_secs(10))
        .map_err(|e| format!("connect {url}: {e}"))?;
    let mut tls = net::tls::connect_with_ca(tcp, &u.host, &ca, None)
        .map_err(|e| format!("tls {url}: {e}"))?;
    let r = net::http::send(&mut tls, &u.host_header(), method, &u.path, headers, body)
        .map_err(|e| format!("{method} {url}: {e}"))?;
    Ok(Resp {
        status: r.status,
        headers: r.headers,
        body: r.body,
    })
}

/// An `application/x-www-form-urlencoded` body (or query) of `pairs`.
pub fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// POST a form.
pub fn post_form(
    url: &str,
    pairs: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Result<Resp, String> {
    let mut all = vec![("Content-Type", "application/x-www-form-urlencoded")];
    all.extend_from_slice(headers);
    request("POST", url, &all, form(pairs).as_bytes())
}

/// A running fake Company.
pub struct Company {
    _server: ConfServer,
    /// `https://127.0.0.1:<port>`.
    pub origin: String,
}

impl Company {
    pub fn spawn(h: &Harness, tmp: &TempDir, variants: &[&str]) -> Company {
        let addr = tmp.path().join(format!("company-{}.addr", random_id()));
        let server = h.spawn_company(&addr, variants);
        let origin = server.endpoint();
        Company {
            _server: server,
            origin,
        }
    }

    /// What the fake recorded of every request that reached it.
    pub fn record(&self) -> Vec<Value> {
        request("GET", &format!("{}/_mock/record", self.origin), &[], &[])
            .map(|r| r.json()["records"].as_array().cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Steer the scripted User (`approver`) and token lifetimes.
    pub fn control(&self, v: Value) {
        let _ = request(
            "POST",
            &format!("{}/_mock/control", self.origin),
            &[("Content-Type", "application/json")],
            v.to_string().as_bytes(),
        );
    }
}

/// Why a Personal Agent does not use a Company's documents.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A fetch failed, or answered something other than JSON.
    Fetch(String),
    /// A redirect to anything but HTTPS (§3).
    Redirect(String),
    /// `organization.domain` is not the host that served the document (§3.1).
    Domain(String),
    /// The metadata's `issuer` is not `auth.issuer`, byte for byte (§3.2).
    Issuer(String),
    /// `poppy_domains` does not include `organization.domain` (§3.2).
    Domains,
    /// No `ES256` among the DPoP algorithms; `prefixed` names the member the
    /// algorithms were published under instead (§3.2, issue #12).
    Es256 { prefixed: Option<String> },
    /// A URL the documents name is off the Company's host (rfcs/0047 §5.5.1).
    OffHost { field: String, host: String },
}

/// A Company's documents, verified.
pub struct Discovered {
    pub poppy: Value,
    pub metadata: Value,
    pub issuer: String,
}

impl Discovered {
    pub fn endpoint(&self, name: &str) -> String {
        self.metadata[name].as_str().unwrap_or_default().to_string()
    }

    pub fn token_endpoint(&self) -> String {
        self.endpoint("token_endpoint")
    }

    /// The `url` of each listed API of `kind`.
    pub fn apis(&self, kind: &str) -> Vec<String> {
        self.poppy["apis"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["type"] == kind)
            .filter_map(|a| a["url"].as_str().map(str::to_string))
            .collect()
    }
}

fn host_of(url: &str) -> String {
    net::http::Url::parse(url)
        .map(|u| u.host)
        .unwrap_or_default()
}

/// The RFC 8414 / RFC 9728 well-known URL of `url`: `suffix` inserted before
/// its path.
pub fn well_known(url: &str, suffix: &str) -> String {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    format!(
        "https://{authority}/.well-known/{suffix}{}",
        path.trim_end_matches('/')
    )
}

/// A Personal Agent's discovery of the Company at `origin` (spec 3, 3.2):
/// the discovery document, following HTTPS redirects only, then the issuer's
/// RFC 8414 metadata, each check the draft asks for applied, and every URL
/// the two documents name required to be on the Company's host.
pub fn discover(origin: &str) -> Result<Discovered, Refused> {
    let mut url = format!("{origin}{DISCOVERY_PATH}");
    let mut hops = 0;
    let poppy = loop {
        let r =
            request("GET", &url, &[("Accept", "application/json")], &[]).map_err(Refused::Fetch)?;
        if (300..400).contains(&r.status) {
            let to = r.header("location").unwrap_or_default().to_string();
            if !to.starts_with("https://") {
                return Err(Refused::Redirect(to));
            }
            hops += 1;
            if hops > 3 {
                return Err(Refused::Fetch("more than 3 redirects".into()));
            }
            url = to;
            continue;
        }
        if r.status != 200 {
            return Err(Refused::Fetch(r.brief()));
        }
        break r.json();
    };
    let served_by = host_of(&url);
    let domain = poppy["organization"]["domain"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if !domain.eq_ignore_ascii_case(&served_by) {
        return Err(Refused::Domain(domain));
    }
    let issuer = poppy["auth"]["issuer"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let m = request(
        "GET",
        &well_known(&issuer, "oauth-authorization-server"),
        &[("Accept", "application/json")],
        &[],
    )
    .map_err(Refused::Fetch)?;
    if m.status != 200 {
        return Err(Refused::Fetch(m.brief()));
    }
    let metadata = m.json();
    if metadata["issuer"] != *issuer {
        return Err(Refused::Issuer(metadata["issuer"].to_string()));
    }
    let listed = metadata[DOMAINS].as_array().is_some_and(|d| {
        d.iter()
            .any(|x| x.as_str().is_some_and(|x| x.eq_ignore_ascii_case(&domain)))
    });
    if !listed {
        return Err(Refused::Domains);
    }
    let es256 = metadata["dpop_signing_alg_values_supported"]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x == "ES256"));
    if !es256 {
        let prefixed = metadata
            .get(PREFIXED_ALGS)
            .map(|_| PREFIXED_ALGS.to_string());
        return Err(Refused::Es256 { prefixed });
    }
    let home = host_of(origin);
    let mut named: Vec<(String, String)> = vec![("auth.issuer".into(), issuer.clone())];
    for f in [
        "token_endpoint",
        "revocation_endpoint",
        "authorization_endpoint",
        "device_authorization_endpoint",
    ] {
        if let Some(u) = metadata[f].as_str() {
            named.push((f.into(), u.to_string()));
        }
    }
    for (i, a) in poppy["apis"].as_array().into_iter().flatten().enumerate() {
        named.push((
            format!("apis[{i}].url"),
            a["url"].as_str().unwrap_or_default().to_string(),
        ));
    }
    for (field, u) in named {
        let host = host_of(&u);
        if host != home {
            return Err(Refused::OffHost { field, host });
        }
    }
    Ok(Discovered {
        poppy,
        metadata,
        issuer,
    })
}

/// The Personal Agent: its keys, and the HTTPS server of its client
/// metadata document and key set, stopped on drop.
pub struct Agent {
    pub key: Key,
    pub dpop: Key,
    pub origin: String,
    pub client_id: String,
    pub redirect_uri: String,
    docs: Arc<Mutex<BTreeMap<String, Value>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Agent {
    /// A Personal Agent with new keys, serving `/client.json` and
    /// `/jwks.json` on a loopback port.
    pub fn start() -> Agent {
        let cert = std::fs::read(tls_fixture("server.pem")).expect("the test server certificate");
        let pem = std::fs::read(tls_fixture("server.key")).expect("the test server key");
        let identity = net::tls::ServerIdentity::from_pem(&cert, &pem).expect("the test identity");
        let acceptor =
            Arc::new(net::tls::TlsAcceptor::new(identity, None).expect("a TLS acceptor"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let origin = format!("https://{}", listener.local_addr().expect("address"));
        let key = Key::generate();
        let client_id = format!("{origin}/client.json");
        let redirect_uri = format!("{origin}/callback");
        let mut docs = BTreeMap::new();
        docs.insert(
            "/client.json".to_string(),
            json!({
                "client_id": client_id,
                "client_name": "agentd conformance",
                "jwks_uri": format!("{origin}/jwks.json"),
                "redirect_uris": [redirect_uri],
                "token_endpoint_auth_method": "private_key_jwt"
            }),
        );
        let mut public = key.jwk.clone();
        public["kid"] = json!(key.kid);
        public["alg"] = json!("ES256");
        public["use"] = json!("sig");
        docs.insert("/jwks.json".to_string(), json!({"keys": [public]}));
        let docs = Arc::new(Mutex::new(docs));
        let stop = Arc::new(AtomicBool::new(false));
        let (served, halt) = (Arc::clone(&docs), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let Ok((tcp, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = tcp.set_nonblocking(false);
                tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let (acceptor, served) = (Arc::clone(&acceptor), Arc::clone(&served));
                std::thread::spawn(move || {
                    let Ok(mut tls) = acceptor.accept(tcp) else {
                        return;
                    };
                    let mut line = String::new();
                    let mut reader = BufReader::new(&mut tls);
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                    }
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let doc = served
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&path)
                        .cloned();
                    let (status, body) = match doc {
                        Some(d) => ("200 OK", d.to_string()),
                        None => ("404 Not Found", String::new()),
                    };
                    let _ = write!(
                        tls,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.flush();
                    let _ = tls.read(&mut [0u8; 1]);
                });
            }
        });
        Agent {
            key,
            dpop: Key::generate(),
            origin,
            client_id,
            redirect_uri,
            docs,
            stop,
        }
    }

    /// Serve `doc` at `path` too, or instead of what was there.
    pub fn serve(&self, path: &str, doc: Value) {
        self.docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string(), doc);
    }

    /// A JWT signed with the client key, its `kid` in the header.
    pub fn signed(&self, claims: &Value) -> String {
        self.key.sign(
            &json!({"alg": "ES256", "typ": "JWT", "kid": self.key.kid}),
            claims,
        )
    }

    /// The claims of a fresh assertion: `iss` the `client_id`, `sub` and
    /// `aud` as given, a minute's life, a new 128-bit `jti`.
    pub fn claims(&self, sub: &str, aud: &str) -> Value {
        let t = now();
        json!({"iss": self.client_id, "sub": sub, "aud": aud, "iat": t, "exp": t + 60, "jti": random_id()})
    }

    /// A client assertion (§4.1): `iss` = `sub` = the `client_id`.
    pub fn client_assertion(&self, aud: &str) -> String {
        self.signed(&self.claims(&self.client_id, aud))
    }

    /// The client authentication fields of a request, with `assertion`.
    pub fn client_auth<'a>(&'a self, assertion: &'a str) -> [(&'a str, &'a str); 3] {
        [
            ("client_id", self.client_id.as_str()),
            ("client_assertion_type", ASSERTION_TYPE),
            ("client_assertion", assertion),
        ]
    }

    /// A DPoP Proof (§4.3) for `method` and `url` — without query or
    /// fragment — with `ath` over `token` and `nonce` when given.
    pub fn proof(
        &self,
        method: &str,
        url: &str,
        token: Option<&str>,
        nonce: Option<&str>,
    ) -> String {
        let mut claims = json!({"jti": random_id(), "htm": method, "htu": url, "iat": now()});
        if let Some(t) = token {
            claims["ath"] = json!(b64url(&sha256(t.as_bytes())));
        }
        if let Some(n) = nonce {
            claims["nonce"] = json!(n);
        }
        self.dpop_proof(&self.dpop, &claims)
    }

    /// A proof with `claims`, signed by `key`, whose JWK is in the header.
    pub fn dpop_proof(&self, key: &Key, claims: &Value) -> String {
        key.sign(
            &json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": key.jwk}),
            claims,
        )
    }

    /// A token request with a fresh client assertion and, unless `dpop` is
    /// false, a fresh proof.
    pub fn token(&self, d: &Discovered, fields: &[(&str, &str)], dpop: bool) -> Resp {
        let at = d.token_endpoint();
        let assertion = self.client_assertion(&d.issuer);
        let mut all: Vec<(&str, &str)> = fields.to_vec();
        all.extend(self.client_auth(&assertion));
        let proof = self.proof("POST", &at, None, None);
        let headers: Vec<(&str, &str)> = if dpop {
            vec![("DPoP", proof.as_str())]
        } else {
            vec![]
        };
        post_form(&at, &all, &headers).unwrap_or_else(|e| Resp {
            status: 0,
            headers: vec![],
            body: e.into_bytes(),
        })
    }

    /// Start a signed-out Session for the User `user` (§4.2): its Session
    /// Token and `session_id`.
    pub fn start_session(&self, d: &Discovered, user: &str) -> Result<(String, String), String> {
        let assertion = self.signed(&self.claims(user, &d.issuer));
        let r = self.token(
            d,
            &[("grant_type", JWT_BEARER), ("assertion", &assertion)],
            true,
        );
        let b = r.json();
        match (
            r.status,
            b["access_token"].as_str(),
            b["session_id"].as_str(),
        ) {
            (200, Some(t), Some(s)) => Ok((t.to_string(), s.to_string())),
            _ => Err(format!("starting a Session: {}", r.brief())),
        }
    }

    /// Sign the Session in through the device flow (§4.6), the scripted
    /// User approving: the token response of the poll that succeeds.
    pub fn device_sign_in(
        &self,
        d: &Discovered,
        session_id: &str,
        scope: &str,
    ) -> Result<Value, String> {
        let at = d.endpoint("device_authorization_endpoint");
        let assertion = self.client_assertion(&d.issuer);
        let mut fields = vec![("scope", scope)];
        fields.extend(self.client_auth(&assertion));
        let r = post_form(&at, &fields, &[])?;
        if r.status != 200 {
            return Err(format!("device authorization: {}", r.brief()));
        }
        let code = r.json()["device_code"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let interval = r.json()["interval"].as_u64().unwrap_or(1);
        for _ in 0..10 {
            std::thread::sleep(Duration::from_secs(interval));
            let p = self.token(
                d,
                &[
                    ("grant_type", DEVICE_CODE),
                    ("device_code", &code),
                    ("session_id", session_id),
                ],
                true,
            );
            match p.status {
                200 => return Ok(p.json()),
                _ if p.error() == "authorization_pending" => continue,
                _ => return Err(format!("device poll: {}", p.brief())),
            }
        }
        Err("the device poll never finished".into())
    }
}
