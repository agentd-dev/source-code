// SPDX-License-Identifier: AGPL-3.0-only
//! agentd as a Personal Agent (rfcs/0047 §9, P1), in process, against the
//! fake Company (`companies::mock`): discovery and each of its refusals, the
//! identity documents, the signed-out Session — assertions, DPoP proofs and
//! the nonce retry — the Bearer token for an MCP server, and what the
//! supervisor does with each answer an MCP server gives. What the fake saw
//! is read from its own record (`/_mock/record`), never taken from agentd.
#![cfg(all(
    feature = "companies",
    any(debug_assertions, feature = "internal-mocks")
))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use agentd::companies::discover::{self, Entry};
use agentd::companies::identity::Identity;
use agentd::companies::mock::{self, Variant};
use agentd::companies::{Supervisor, wire};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../net/tests/fixtures")
        .join(name)
}

/// Trust the test PKI's CA for every dial this process makes.
fn trust_the_test_ca() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let ca = std::fs::read(fixture("ca.pem")).unwrap();
        agentd::net::tls::install_extra_ca(&ca).unwrap();
    });
}

fn get(url: &str) -> (u16, Value) {
    let r = agentd::auth::http::get(
        url,
        &[],
        Duration::from_secs(10),
        agentd::auth::http::Dial::Named,
    )
    .unwrap_or_else(|e| panic!("GET {url}: {e}"));
    (
        r.status,
        serde_json::from_slice(&r.body).unwrap_or(Value::Null),
    )
}

/// A fake Company on a loopback port of its own, in a thread of this
/// process.
struct Fake {
    origin: String,
}

fn fake(variants: &[Variant]) -> Fake {
    trust_the_test_ca();
    let dir = tempfile::tempdir().unwrap().keep();
    let addr = dir.join("addr");
    let opts = mock::Options {
        cert: fixture("server.pem").display().to_string(),
        key: fixture("server.key").display().to_string(),
        ca: Some(fixture("ca.pem").display().to_string()),
        variants: variants.to_vec(),
        listen: None,
    };
    let at = addr.display().to_string();
    std::thread::spawn(move || mock::run_with(&at, &opts));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !addr.exists() {
        assert!(Instant::now() < deadline, "the fake never announced itself");
        std::thread::sleep(Duration::from_millis(10));
    }
    let bound = std::fs::read_to_string(&addr).unwrap();
    let port = bound.rsplit(':').next().unwrap();
    Fake {
        origin: format!("https://127.0.0.1:{port}"),
    }
}

impl Fake {
    /// What reached the fake, its own steering and reading aside.
    fn record(&self) -> Vec<Value> {
        let (_, v) = get(&format!("{}/_mock/record", self.origin));
        v["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                !r["path"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("/_mock/")
            })
            .cloned()
            .collect()
    }

    fn control(&self, v: Value) {
        agentd::auth::http::send(
            "POST",
            &format!("{}/_mock/control", self.origin),
            &[("Content-Type", "application/json")],
            v.to_string().as_bytes(),
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
    }

    fn entry(&self, name: &str) -> Entry {
        Entry {
            name: name.into(),
            endpoint: self.origin.clone(),
            hosts: None,
            allow_private: true,
            timeout: Duration::from_secs(10),
            servers: vec![(name.into(), None)],
            pace: Default::default(),
            sign_in: Vec::new(),
            scopes: Vec::new(),
            interfaces: vec![agentd::config::settings::companies::Interface::Mcp],
            api_allow: Vec::new(),
        }
    }
}

/// An HTTPS server of the client's documents, by path, on a port of its own:
/// what static hosting of `--print-company-client`'s output is.
struct Host {
    origin: String,
    docs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

fn host() -> Host {
    trust_the_test_ca();
    let identity = agentd::net::tls::ServerIdentity::from_pem(
        &std::fs::read(fixture("server.pem")).unwrap(),
        &std::fs::read(fixture("server.key")).unwrap(),
    )
    .unwrap();
    let acceptor = Arc::new(agentd::net::tls::TlsAcceptor::new(identity, None).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let docs: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    let served = Arc::clone(&docs);
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let (acceptor, served) = (Arc::clone(&acceptor), Arc::clone(&served));
            std::thread::spawn(move || {
                let Ok(mut tls) = acceptor.accept(tcp) else {
                    return;
                };
                let mut line = String::new();
                let mut r = BufReader::new(&mut tls);
                if r.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let doc = served.lock().unwrap().get(&path).cloned();
                let (status, body) = match doc {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(
                    tls,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(&body);
                let _ = tls.flush();
            });
        }
    });
    Host { origin, docs }
}

impl Host {
    fn publish(&self, identity: &Identity) {
        let mut d = self.docs.lock().unwrap();
        for (path, body) in identity.documents() {
            d.insert(path, body);
        }
    }
}

/// An agent: its configuration in a scratch dir, its identity published on
/// `host`, its store `store`, and the supervisor built from them.
struct Agent {
    sup: Arc<Supervisor>,
    _dir: PathBuf,
}

/// Its conversations' readers stop with it.
impl Drop for Agent {
    fn drop(&mut self) {
        self.sup.stop_readers();
    }
}

fn config(host: &Host, fake: &Fake, dir: &Path, extra: &str) -> agentd::config::settings::Settings {
    let yaml = format!(
        "security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"{h}/companies/client.json\", name: \"Test agent\", key_file: \"{d}/client-key\"}}\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         services:\n  shop: {{kind: company, endpoint: \"{o}\", allow_private: true{extra}}}\n",
        ca = fixture("ca.pem").display(),
        h = host.origin,
        d = dir.display(),
        o = fake.origin,
    );
    let path = dir.join("c.yaml");
    std::fs::write(&path, yaml).unwrap();
    agentd::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
        .unwrap_or_else(|e| panic!("{e}"))
        .0
        .settings
}

fn logger() -> agentd::obs::log::Logger {
    agentd::obs::log::Logger::new(
        agentd::obs::log::LogCtx {
            run_id: "t".into(),
            agent_id: "sup".into(),
            agent_path: "0".into(),
            comp: agentd::obs::log::Comp::Supervisor,
            pid: std::process::id(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            trace_flags: None,
        },
        agentd::obs::log::Level::Error,
    )
}

fn agent_on(
    fake: &Fake,
    host: &Host,
    dir: &Path,
    store: Arc<agentd::store::memory::MemoryStore>,
    extra: &str,
) -> Agent {
    let settings = config(host, fake, dir, extra);
    let durable = Arc::new(agentd::state::Durable::new(
        store,
        "t/",
        "t",
        agentd::state::Policy::default(),
        None,
    ));
    let sup = Supervisor::build(&settings, durable, &logger())
        .unwrap()
        .expect("a Company is declared");
    host.publish(&sup.identity);
    Agent {
        sup,
        _dir: dir.to_path_buf(),
    }
}

fn agent(fake: &Fake) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    agent_on(fake, &host(), &dir, Arc::default(), "")
}

fn tokens(record: &[Value]) -> Vec<&Value> {
    record
        .iter()
        .filter(|r| r["path"] == "/auth/token")
        .collect()
}

fn presented(token: &str) -> String {
    let h = agentd::sha::sha256(token.as_bytes());
    let mut s = String::new();
    // base64url, unpadded: the record names a token by the first eight.
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    for chunk in h.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            s.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    s.truncate(8);
    s
}

/// Each check of discovery refuses the variant built to break it, with its
/// named reason, and the conforming fake verifies.
#[test]
fn each_discovery_check_refuses_its_flawed_variant() {
    let good = fake(&[]);
    let v = discover::discover(&good.entry("shop")).unwrap();
    assert_eq!(v.domain, "127.0.0.1");
    assert!(v.issuer.ends_with("/auth"));
    assert_eq!(v.mcp["shop"], format!("{}/mcp", good.origin));
    let asked: Vec<String> = good
        .record()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        asked.contains(&format!("/.well-known/{}/auth", wire::AS_METADATA_SUFFIX)),
        "the RFC 8414 suffix goes before the issuer's path: {asked:?}"
    );
    let refused = |variant: Variant| {
        discover::discover(&fake(&[variant]).entry("shop")).expect_err(variant.name())
    };
    assert_eq!(refused(Variant::IssuerMismatch).reason, "issuer");
    assert_eq!(refused(Variant::NoDomains).reason, "domains");
    let e = refused(Variant::NoEs256);
    assert_eq!(e.reason, "es256");
    assert!(e.detail.contains("does not include ES256"), "{}", e.detail);
    let e = refused(Variant::PrefixedAlgs);
    assert_eq!(e.reason, "es256");
    assert!(
        e.detail.contains(wire::PREFIXED_DPOP_ALGS_MEMBER),
        "{}",
        e.detail
    );
    let e = refused(Variant::OffHostEndpoints);
    assert_eq!(e.reason, "endpoint");
    assert_eq!(e.host.as_deref(), Some("localhost"));
    assert!(
        e.field.as_deref().is_some_and(|f| f.ends_with("_endpoint")),
        "{e:?}"
    );
    let e = refused(Variant::RedirectToHttp);
    assert_eq!(e.reason, "redirect");
    assert!(e.detail.contains("http://"), "{}", e.detail);
    assert_eq!(refused(Variant::TwoMcpApis).reason, "mcp_names");
}

/// The host rule and the SSRF guard hold for every Company dial: an
/// endpoint outside the entry's `hosts` is refused before a socket opens,
/// and a private address without `allow_private`.
#[test]
fn an_endpoint_outside_hosts_and_a_private_address_are_refused() {
    let f = fake(&[]);
    let mut entry = f.entry("shop");
    entry.hosts = Some(vec!["example.com".into()]);
    let e = discover::discover(&entry).unwrap_err();
    assert_eq!(e.reason, "endpoint", "{e:?}");
    assert_eq!(e.host.as_deref(), Some("127.0.0.1"));
    let mut entry = f.entry("shop");
    entry.allow_private = false;
    let e = discover::discover(&entry).unwrap_err();
    assert_eq!(e.reason, "fetch", "{e:?}");
    assert!(e.detail.contains("loopback"), "{}", e.detail);
    assert!(
        f.record().is_empty(),
        "nothing reached the fake: {:?}",
        f.record()
    );
}

/// Every document agentd publishes holds to the kit's
/// `client-metadata.schema.json`, `redirect_uris` included.
#[test]
fn every_published_identity_document_validates_against_the_kit() {
    let dir = tempfile::tempdir().unwrap();
    let client = agentd::config::settings::companies::Client {
        id: "https://agent.example/companies/client.json".into(),
        name: "Test agent".into(),
        logo_uri: Some("https://agent.example/logo.png".into()),
        jwks_uri: None,
        key_file: Some(dir.path().join("key").display().to_string()),
        published_key_files: Vec::new(),
        redirect_uri: None,
        serve: true,
        extensions: Vec::new(),
    };
    let id = Identity::load(&client, false).unwrap();
    let kit = mock::kit::kit();
    kit.validate("client-metadata.schema.json", id.metadata())
        .unwrap();
    assert_eq!(id.metadata()["redirect_uris"], json!([]));
    // Listing the Direct Sign-In callback, it holds to the kit too.
    let direct = Identity::load(&client, true).unwrap();
    kit.validate("client-metadata.schema.json", direct.metadata())
        .unwrap();
    assert_eq!(
        direct.metadata()["redirect_uris"],
        json!(["https://agent.example/companies/callback"])
    );
    for (path, body) in id.documents() {
        let v: Value = serde_json::from_slice(&body).unwrap();
        if path.ends_with("client.json") {
            kit.validate("client-metadata.schema.json", &v).unwrap();
        } else {
            assert!(v["keys"][0]["kid"] == id.kid());
        }
    }
    for f in id.print(&dir.path().join("out")).unwrap() {
        let v: Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        if f.ends_with("client.json") {
            kit.validate("client-metadata.schema.json", &v).unwrap();
        }
    }
    // Listing the Operations extension, it holds to the kit too.
    let listing = Identity::load(
        &agentd::config::settings::companies::Client {
            extensions: vec![agentd::config::settings::companies::Extension::Operations],
            ..client
        },
        false,
    )
    .unwrap();
    kit.validate("client-metadata.schema.json", listing.metadata())
        .unwrap();
    assert_eq!(
        listing.metadata()["extensions"]["operations"]["version"],
        "1"
    );
}

/// A Session, a Bearer token and the MCP server's tools: the fake accepts
/// the assertions only as the draft specifies them, every proof passes its
/// checks, the Bearer token is requested with `resource` and no `DPoP`
/// header and is presented only to its MCP server, and every part of every
/// request that has a schema was held to it.
#[test]
fn a_signed_out_session_reaches_the_mcp_server_with_a_bearer_token_for_it() {
    let f = fake(&[]);
    let a = agent(&f);
    let (client, tools) = agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20))
        .unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search", "order_status", "exchange_item"]);
    assert!(
        tools[0]
            .description
            .as_deref()
            .unwrap()
            .starts_with("[A tool of \"Fake Company\" (127.0.0.1)"),
        "{:?}",
        tools[0].description
    );
    let r = client
        .call_tool("search", Some(json!({"query": "boots"})))
        .unwrap();
    assert!(!r.is_error());
    let record = f.record();
    let issuer = format!("{}/auth", f.origin);
    let client_id = a.sup.identity.client_id.clone();
    let mcp = format!("{}/mcp", f.origin);
    let toks = tokens(&record);
    assert!(toks.len() >= 2, "{record:#?}");
    for t in &toks {
        assert_eq!(t["status"], 200, "{t}");
        assert_eq!(t["grant"], wire::grant::JWT_BEARER);
        let ca = &t["client_assertion"];
        assert_eq!(ca["iss"], client_id.as_str());
        assert_eq!(ca["sub"], client_id.as_str());
        assert_eq!(ca["aud"], issuer.as_str(), "aud is the issuer, one string");
        assert_eq!(
            ca["exp"].as_u64().unwrap() - ca["iat"].as_u64().unwrap(),
            60
        );
        let sa = &t["assertion"];
        assert_eq!(sa["iss"], client_id.as_str());
        assert_eq!(sa["aud"], issuer.as_str());
        assert!(sa["sub"].as_str().unwrap().starts_with("usr_"));
        let v = t["validated"].as_array().unwrap();
        assert!(v.contains(&json!("session-assertion.schema.json")), "{t}");
    }
    // The first starts a Session with a proof; the Bearer request names the
    // MCP server's url, carries the session_id and no proof.
    assert_eq!(toks[0]["dpop"], true);
    assert_eq!(toks[0]["resource"], Value::Null);
    assert_eq!(toks[0]["session_id"], Value::Null);
    assert!(
        toks[0]["validated"]
            .as_array()
            .unwrap()
            .contains(&json!("dpop-proof.schema.json")),
        "{}",
        toks[0]
    );
    assert!(
        toks[0]["proof"].get("ath").is_none(),
        "no ath to the token endpoint"
    );
    let bearer = toks
        .iter()
        .find(|t| t["resource"] == mcp.as_str())
        .expect("a Bearer token for the MCP server");
    assert_eq!(bearer["dpop"], false, "requested without the DPoP header");
    assert!(bearer["session_id"].is_string());
    assert!(bearer.get("proof").is_none());
    // Presented to its server and nowhere else.
    let token = bearer["issued"]["access_token"].as_str().unwrap();
    let mark = presented(token);
    let at: Vec<&str> = record
        .iter()
        .filter(|r| r["presented"] == mark.as_str())
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(!at.is_empty() && at.iter().all(|p| *p == "/mcp"), "{at:?}");
    // The client metadata document the fake fetched held to the kit.
    assert!(
        record.iter().any(|r| r["validated"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("client-metadata.schema.json")))),
        "{record:#?}"
    );
    // Every proof passed §4.3's checks: nothing was refused.
    assert!(record.iter().all(|r| r["error"].is_null()), "{record:#?}");
}

/// A Company that requires nonces answers the first proof `use_dpop_nonce`;
/// the retry carries its nonce, and fresh assertions and a fresh proof.
#[test]
fn use_dpop_nonce_is_retried_once_with_fresh_assertions() {
    let f = fake(&[Variant::NonceRequired]);
    let a = agent(&f);
    agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20))
        .unwrap_or_else(|e| panic!("{e}"));
    let record = f.record();
    let toks = tokens(&record);
    assert_eq!(toks[0]["error"], wire::error::USE_DPOP_NONCE, "{record:#?}");
    assert_eq!(toks[1]["status"], 200, "{record:#?}");
    assert!(
        toks[1]["proof"]["nonce"].is_string(),
        "the retry carries the nonce"
    );
    for key in ["client_assertion", "assertion", "proof"] {
        assert_ne!(
            toks[0][key]["jti"], toks[1][key]["jti"],
            "a fresh {key} jti on the retry"
        );
    }
    assert_eq!(
        toks.iter()
            .filter(|t| t["error"] == wire::error::USE_DPOP_NONCE)
            .count(),
        1,
        "retried once"
    );
}

/// `invalid_token` renews the Bearer token and retries the request once;
/// a server that keeps refusing gets exactly one retry.
#[test]
fn invalid_token_renews_and_retries_once() {
    let f = fake(&[]);
    let a = agent(&f);
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    let calls = |record: &[Value]| -> Vec<Value> {
        record
            .iter()
            .filter(|r| {
                r["mcp"] == "tools/call"
                    || (r["path"] == "/mcp" && r["error"] == wire::error::INVALID_TOKEN)
            })
            .cloned()
            .collect()
    };
    let before = f.record().len();
    f.control(json!({"refuse_tokens": 1}));
    let r = client
        .call_tool("search", Some(json!({"query": "x"})))
        .unwrap();
    assert!(!r.is_error());
    let record = f.record();
    let after = &record[before..];
    let mcp: Vec<&Value> = after.iter().filter(|r| r["path"] == "/mcp").collect();
    assert_eq!(mcp.len(), 2, "one refused, one retried: {after:#?}");
    assert_eq!(mcp[0]["error"], wire::error::INVALID_TOKEN);
    assert_eq!(mcp[1]["status"], 200);
    assert_ne!(
        mcp[0]["presented"], mcp[1]["presented"],
        "the retry carries a new token"
    );
    assert!(
        after
            .iter()
            .any(|r| r["path"] == "/auth/token" && r["resource"].is_string()),
        "the token was renewed"
    );
    // One that keeps refusing is retried once, and the call fails.
    let before = f.record().len();
    f.control(json!({"refuse_tokens": 10}));
    let out = client.call_tool("search", Some(json!({"query": "x"})));
    assert!(out.is_err() || out.as_ref().unwrap().is_error(), "{out:?}");
    let record = f.record();
    let refused = calls(&record[before..]);
    assert_eq!(refused.len(), 2, "one retry: {refused:#?}");
}

/// `403 sign_in_required` with no Account Token is a typed tool error, and
/// `extension_required` marks the server unusable: what follows is refused
/// unsent.
#[test]
fn sign_in_required_is_typed_and_extension_required_marks_the_server_unusable() {
    let f = fake(&[]);
    let a = agent(&f);
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    let text = match client.call_tool("order_status", Some(json!({"order_id": "o1"}))) {
        Ok(r) => r.text(),
        Err(e) => e.to_string(),
    };
    assert!(text.contains(wire::error::SIGN_IN_REQUIRED), "{text}");
    let typed = agentd::companies::mcp::typed_error("shop", &text);
    assert!(
        typed.contains("company shop needs a signed-in Session"),
        "{typed}"
    );
    f.control(json!({"extension_required": "operations"}));
    let text = match client.call_tool("search", Some(json!({"query": "x"}))) {
        Ok(r) => r.text(),
        Err(e) => e.to_string(),
    };
    assert!(text.contains(wire::error::EXTENSION_REQUIRED), "{text}");
    let company = a.sup.company("shop").unwrap();
    assert!(company.is_unusable(&format!("{}/mcp", f.origin)));
    let before = f.record().len();
    let text = match client.call_tool("search", Some(json!({"query": "x"}))) {
        Ok(r) => r.text(),
        Err(e) => e.to_string(),
    };
    assert!(text.contains("refused this agent"), "{text}");
    assert_eq!(f.record().len(), before, "refused unsent");
}

/// After a restart the Company sees the same User ID and the same
/// `session_id`, renewed with a new DPoP key.
#[test]
fn a_restart_keeps_the_user_id_and_the_session_with_a_new_dpop_key() {
    let f = fake(&[]);
    let dir = tempfile::tempdir().unwrap().keep();
    let h = host();
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let first = agent_on(&f, &h, &dir, Arc::clone(&store), "");
    agentd::companies::mcp::connect(&first.sup, "shop", Duration::from_secs(20)).unwrap();
    let life1 = tokens(&f.record()).into_iter().cloned().collect::<Vec<_>>();
    let started = life1.iter().find(|t| t["dpop"] == true).unwrap().clone();
    let sid = life1.iter().find(|t| t["resource"].is_string()).unwrap()["session_id"].clone();
    drop(first);
    let before = f.record().len();
    let second = agent_on(&f, &h, &dir, store, "");
    agentd::companies::mcp::connect(&second.sup, "shop", Duration::from_secs(20)).unwrap();
    let record = f.record();
    let life2: Vec<&Value> = tokens(&record[before..]);
    let renewed = life2.iter().find(|t| t["dpop"] == true).unwrap();
    assert_eq!(renewed["session_id"], sid, "the same Session, renewed");
    assert_eq!(renewed["status"], 200);
    assert_eq!(
        renewed["assertion"]["sub"], started["assertion"]["sub"],
        "the same User ID"
    );
    assert_ne!(renewed["jkt"], started["jkt"], "a new DPoP key");
    assert!(life2.iter().all(|t| t["session_id"] == sid), "{life2:#?}");
}

/// A Session the Company ended is started again, and kept.
#[test]
fn an_ended_session_is_started_again() {
    let f = fake(&[]);
    let a = agent(&f);
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    let before = f.record().len();
    f.control(json!({"end_sessions": true, "refuse_tokens": 1}));
    let r = client
        .call_tool("search", Some(json!({"query": "x"})))
        .unwrap();
    assert!(!r.is_error());
    let record = f.record();
    let toks = tokens(&record[before..]);
    assert!(
        toks.iter()
            .any(|t| t["error"] == wire::error::INVALID_SESSION),
        "{toks:#?}"
    );
    assert!(
        toks.iter()
            .any(|t| t["dpop"] == true && t["session_id"].is_null() && t["status"] == 200),
        "a new Session: {toks:#?}"
    );
}

/// A Company tool registers only as `<server>.<tool>`: one called
/// `send_email` never takes the bare name another server's tool has, in
/// either registration order.
#[test]
fn a_company_tool_never_takes_a_bare_name() {
    use agentd::registry::{Registry, Route, ServerTools};
    let dir = tempfile::tempdir().unwrap();
    let yaml = format!(
        "security:\n  companies:\n    client: {{id: \"https://agent.example/c.json\", name: A, key_file: \"{d}/k\"}}\n\
         store: {{kind: file, file: {{path: \"{d}/s\"}}}}\n\
         services:\n  shop: {{kind: company, endpoint: \"https://example.com\"}}\n\
         mcp:\n  servers:\n    - {{name: mail, endpoint: \"https://mail.example/mcp\"}}\n",
        d = dir.path().display()
    );
    let path = dir.path().join("c.yaml");
    std::fs::write(&path, yaml).unwrap();
    let settings =
        agentd::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .unwrap()
            .0
            .settings;
    let tool = |name: &str| ::mcp::wire::Tool {
        name: name.into(),
        title: None,
        description: Some(format!("{name} tool")),
        input_schema: json!({"type": "object"}),
        output_schema: None,
    };
    let server = |name: &str| {
        let s = settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == name)
            .unwrap();
        ServerTools {
            name: name.into(),
            ns: s.ns.clone(),
            tags: s.tag_set().unwrap(),
            tools: vec![tool("send_email")],
        }
    };
    for order in [
        [server("shop"), server("mail")],
        [server("mail"), server("shop")],
    ] {
        let reg = Registry::build(&settings, &order).unwrap();
        assert_eq!(
            reg.route("send_email"),
            Some(Route::Mcp {
                server: "mail",
                tool: "send_email"
            }),
            "the bare name is the other server's"
        );
        assert_eq!(
            reg.route("shop.send_email"),
            Some(Route::Mcp {
                server: "shop",
                tool: "send_email"
            })
        );
    }
}

/// Wait until `pred` holds of what the fake recorded, or fail saying `what`.
fn recorded(f: &Fake, what: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let r = f.record();
        if pred(&r) {
            return r;
        }
        assert!(Instant::now() < deadline, "never {what}: {r:#?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn count(record: &[Value], method: &str) -> usize {
    record.iter().filter(|r| r["mcp"] == method).count()
}

/// A `429` with `Retry-After` from the Company holds every task: the token
/// endpoint's holds the next connection as much as its own retry, and a
/// `tools/call`'s holds a second task on a connection of its own, which
/// sends nothing — not one request reaches the fake before the wait is over
/// (draft §4.3: "MUST wait at least that long").
#[test]
fn two_tasks_send_nothing_before_the_companys_retry_after() {
    let f = fake(&[Variant::RetryAfter]);
    let a = agent(&f);
    let company = a.sup.company("shop").unwrap();
    let e = agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(10))
        .err()
        .expect("the first token request is answered 429");
    assert!(e.to_string().contains("rate_limited"), "{e}");
    let until = company.pace().until().expect("the Company's wait is kept");
    // Another task, at once: refused, unsent.
    let before = f.record().len();
    let sup = Arc::clone(&a.sup);
    let e = std::thread::spawn(move || {
        agentd::companies::mcp::connect(&sup, "shop", Duration::from_secs(10))
            .err()
            .expect("held")
    })
    .join()
    .unwrap();
    assert!(e.to_string().contains("rate limited by shop until"), "{e}");
    assert_eq!(f.record().len(), before, "nothing was sent: {e}");
    let wait = until
        .duration_since(std::time::SystemTime::now())
        .unwrap_or_default();
    std::thread::sleep(wait + Duration::from_millis(100));
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(10)).unwrap();
    // The first `tools/call` is answered 429: a second task's own
    // connection is refused unsent.
    let e = client
        .call_tool("search", Some(json!({"query": "boots"})))
        .expect_err("429");
    assert!(e.to_string().contains("429"), "{e}");
    assert!(company.pace().until().is_some());
    let before = f.record().len();
    let sup = Arc::clone(&a.sup);
    let e = std::thread::spawn(move || {
        agentd::companies::mcp::connect(&sup, "shop", Duration::from_secs(10))
            .err()
            .expect("held")
    })
    .join()
    .unwrap();
    assert!(e.to_string().contains("rate limited by shop until"), "{e}");
    assert_eq!(f.record().len(), before, "nothing was sent: {e}");
    let record = f.record();
    assert!(
        record.iter().all(|r| r["early"] != true),
        "a request reached the fake before its wait was over: {record:#?}"
    );
    assert_eq!(count(&record, "tools/call"), 1);
}

/// The handshake and the tool list are safe to repeat: a `tools/list` that
/// gets no answer in time is asked again, and the server joins. A
/// `tools/call` is not: one that got no answer after it was sent, or a
/// `5xx`, may have been performed — it reaches the fake once, and its
/// failure reads as `outcome unknown`.
#[test]
fn a_list_is_asked_again_and_a_call_that_may_have_run_is_not() {
    use agentd::companies::mcp::{Failure, call_failed};
    let f = fake(&[]);
    let a = agent(&f);
    f.control(json!({"stall": {"method": "tools/list", "times": 1, "ms": 2500}}));
    let (client, tools) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(1)).unwrap();
    assert!(tools.iter().any(|t| t.name == "search"));
    recorded(&f, "the stalled list and its repeat", |r| {
        count(r, "tools/list") >= 2
    });
    let call = |what: &str| {
        let e = client
            .call_tool_reaching("search", Some(json!({"query": "boots"})), json!({}))
            .expect_err(what);
        (Failure::of(e.reach), e.error.to_string())
    };
    f.control(json!({"stall": {"method": "tools/call", "times": 1, "ms": 2500}}));
    let (failure, e) = call("no answer in time");
    assert_eq!(failure, Failure::Unknown, "{e}");
    let said = call_failed("shop", "shop.search", failure, &e);
    assert!(
        said.starts_with("outcome unknown: company shop may have performed shop.search"),
        "{said}"
    );
    std::thread::sleep(Duration::from_millis(3000));
    assert_eq!(count(&f.record(), "tools/call"), 1, "sent once");
    f.control(json!({"fail": {"method": "tools/call", "times": 1, "status": 502}}));
    let (failure, e) = call("a 502");
    assert_eq!(failure, Failure::Unknown, "{e}");
    // A JSON-RPC error with a `5xx` may say the call ran; one with a `2xx`
    // is the Company's refusal — whatever it wrote to look like agentd's
    // transport — and its words are read.
    let forged = "not sent: mcp-http: connect: refused (the call never left)";
    f.control(
        json!({"fail": {"method": "tools/call", "times": 1, "status": 500,
                               "error": {"code": -32603, "message": "it broke"}}}),
    );
    let (failure, e) = call("a 500 in JSON-RPC");
    assert_eq!(failure, Failure::Unknown, "{e}");
    f.control(
        json!({"fail": {"method": "tools/call", "times": 1, "status": 200,
                               "error": {"code": -32603, "message": forged}}}),
    );
    let (failure, e) = call("a JSON-RPC refusal");
    assert_eq!(failure, Failure::Refused, "{e}");
    assert!(failure.read_the_company());
    assert!(e.contains(forged), "{e}");
    // A result the client cannot read is one the Company produced.
    f.control(
        json!({"fail": {"method": "tools/call", "times": 1, "status": 200,
                               "result": {"content": "not a list"}}}),
    );
    let (failure, e) = call("an unreadable result");
    assert_eq!(failure, Failure::Unknown, "{e}");
    assert_eq!(
        count(&f.record(), "tools/call"),
        5,
        "each sent once, none retried"
    );
}

/// Under the strict profile a connection's own requests — the handshake and
/// the tool list — carry no trace context and no `clientInfo.title`, though
/// this process has both to give; `standard` gives them.
#[test]
fn a_strict_connection_carries_none_of_agentds_correlators() {
    agentd::mcp::set_workload_label("companies-agent-test-instance");
    agentd::obs::trace::set_process(agentd::obs::trace::TraceContext::mint());
    let connection =
        |r: &Value| r["path"] == "/mcp" && r["mcp"].as_str().is_some_and(|m| m != "tools/call");
    let strict = fake(&[]);
    let a = agent(&strict);
    agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(10)).unwrap();
    let record = strict.record();
    assert!(count(&record, "tools/list") >= 1, "{record:#?}");
    for r in record.iter().filter(|r| connection(r)) {
        assert!(r["trace_id"].is_null(), "{r}");
        assert!(r["client_title"].is_null(), "{r}");
        assert!(r["aauth"].is_null() && r["trace_header"].is_null(), "{r}");
    }
    let standard = fake(&[]);
    let dir = tempfile::tempdir().unwrap().keep();
    let b = agent_on(
        &standard,
        &host(),
        &dir,
        Arc::default(),
        ", privacy: standard",
    );
    agentd::companies::mcp::connect(&b.sup, "shop", Duration::from_secs(10)).unwrap();
    let record = standard.record();
    let listed = record
        .iter()
        .find(|r| r["mcp"] == "tools/list")
        .expect("listed");
    assert!(listed["trace_id"].is_string(), "{listed}");
    assert_eq!(
        listed["client_title"], "companies-agent-test-instance",
        "{listed}"
    );
}

/// An agent acting at `shop` and `travel`, its User ID records sealed with
/// the key of 32 `byte`s.
fn sealed_agent(
    h: &Host,
    shop: &Fake,
    travel: &Fake,
    dir: &Path,
    store: Arc<agentd::store::memory::MemoryStore>,
    byte: u8,
) -> Agent {
    let key = dir.join(format!("seal-{byte}"));
    std::fs::write(&key, format!("{byte:02x}").repeat(32)).unwrap();
    let yaml = format!(
        "security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"{h}/companies/client.json\", name: \"Test agent\", key_file: \"{d}/client-key\"}}\n    seal_key: \"{{{{secret-file:{k}}}}}\"\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         services:\n  shop: {{kind: company, endpoint: \"{s}\", allow_private: true}}\n  travel: {{kind: company, endpoint: \"{t}\", allow_private: true}}\n",
        ca = fixture("ca.pem").display(),
        h = h.origin,
        d = dir.display(),
        k = key.display(),
        s = shop.origin,
        t = travel.origin,
    );
    let path = dir.join(format!("c-{byte}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    let settings =
        agentd::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .unwrap_or_else(|e| panic!("{e}"))
            .0
            .settings;
    let durable = Arc::new(agentd::state::Durable::new(
        store,
        "t/",
        "t",
        agentd::state::Policy::default(),
        None,
    ));
    let sup = Supervisor::build(&settings, durable, &logger())
        .unwrap()
        .expect("two Companies are declared");
    h.publish(&sup.identity);
    Agent {
        sup,
        _dir: dir.to_path_buf(),
    }
}

/// A seal key that opens none of the store's records makes every Company
/// unavailable (RFC 0047 §5.5.3). Resetting one starts that one over as a
/// new User and leaves the other unavailable until it is reset too; then
/// the store takes the key, and what was started over is kept.
#[test]
fn a_reset_with_another_seal_key_starts_over_one_company_at_a_time() {
    let (shop, travel) = (fake(&[]), fake(&[]));
    let dir = tempfile::tempdir().unwrap().keep();
    let h = host();
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let connect = |a: &Agent, server: &str| {
        agentd::companies::mcp::connect(&a.sup, server, Duration::from_secs(20)).map(|_| ())
    };
    let user = |f: &Fake| {
        tokens(&f.record())
            .last()
            .map(|t| t["assertion"]["sub"].clone())
            .unwrap()
    };
    let first = sealed_agent(&h, &shop, &travel, &dir, Arc::clone(&store), 1);
    connect(&first, "shop").unwrap();
    connect(&first, "travel").unwrap();
    let (shop_user, travel_user) = (user(&shop), user(&travel));
    drop(first);

    let second = sealed_agent(&h, &shop, &travel, &dir, Arc::clone(&store), 2);
    for server in ["shop", "travel"] {
        let e = connect(&second, server).expect_err("a key that opens nothing");
        assert!(e.to_string().contains("seal_key"), "{server}: {e}");
    }
    let company = |name: &str| second.sup.company(name).unwrap();
    let r = second.sup.reset_user(&company("shop")).unwrap();
    assert_eq!(r["started_over"], true, "{r}");
    connect(&second, "shop").unwrap();
    let new_shop_user = user(&shop);
    assert_ne!(new_shop_user, shop_user, "a new User at shop");
    let e = connect(&second, "travel").expect_err("travel is not reset");
    assert!(e.to_string().contains("seal_key"), "{e}");
    assert_eq!(user(&travel), travel_user, "nothing was sent to travel");
    let r = second.sup.reset_user(&company("travel")).unwrap();
    assert_eq!(r["started_over"], true, "{r}");
    connect(&second, "travel").unwrap();
    assert_ne!(user(&travel), travel_user, "a new User at travel");
    drop(second);

    // The store took the key: a restart with it opens both records.
    let third = sealed_agent(&h, &shop, &travel, &dir, store, 2);
    connect(&third, "shop").unwrap();
    assert_eq!(user(&shop), new_shop_user, "the User started over is kept");
}

// ---- Signing in (RFC 0047 P2) ---------------------------------------------

use agentd::companies::signin::{Ask, Outcome, Started};
use std::sync::atomic::AtomicBool;

/// An agent that may sign the User in at `fake` with the sign-in types
/// `kinds` and the scopes `scopes` — or, with no `kinds`, may sign nobody
/// in — its Account Tokens sealed with a seal key in `dir`; `client` adds to
/// the client identity and `top` to the configuration.
fn signing_config(
    host: &Host,
    fake: &Fake,
    dir: &Path,
    kinds: &str,
    scopes: &str,
    client: &str,
    top: &str,
) -> agentd::config::settings::Settings {
    std::fs::write(dir.join("seal"), "5e".repeat(32)).unwrap();
    let signs_in = if kinds.is_empty() {
        String::new()
    } else {
        format!(", sign_in: {kinds}, scopes: {scopes}")
    };
    let yaml = format!(
        "security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"{h}/companies/client.json\", name: \"Test agent\", key_file: \"{d}/client-key\"{client}}}\n    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         services:\n  shop: {{kind: company, endpoint: \"{o}\", allow_private: true{signs_in}}}\n{top}",
        ca = fixture("ca.pem").display(),
        h = host.origin,
        d = dir.display(),
        o = fake.origin,
    );
    let path = dir.join("c.yaml");
    std::fs::write(&path, yaml).unwrap();
    agentd::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
        .unwrap_or_else(|e| panic!("{e}"))
        .0
        .settings
}

fn supervisor_of(
    settings: &agentd::config::settings::Settings,
    store: Arc<agentd::store::memory::MemoryStore>,
    host: &Host,
) -> Arc<Supervisor> {
    supervisor_with(settings, store, host)
}

/// [`supervisor_of`] on any store.
fn supervisor_with(
    settings: &agentd::config::settings::Settings,
    store: Arc<dyn agentd::store::Store>,
    host: &Host,
) -> Arc<Supervisor> {
    let durable = Arc::new(agentd::state::Durable::new(
        store,
        "t/",
        "t",
        agentd::state::Policy::default(),
        None,
    ));
    let sup = Supervisor::build(settings, durable, &logger())
        .unwrap()
        .expect("a Company is declared");
    host.publish(&sup.identity);
    sup
}

/// [`signing_config`]'s agent, on `store`, signing in by Device Sign-In for
/// `scopes`.
fn signing_on(
    fake: &Fake,
    host: &Host,
    dir: &Path,
    store: Arc<agentd::store::memory::MemoryStore>,
    scopes: &str,
) -> Agent {
    let settings = signing_config(host, fake, dir, "[device]", scopes, "", "");
    Agent {
        sup: supervisor_of(&settings, store, host),
        _dir: dir.to_path_buf(),
    }
}

const BOTH: &str = "[\"poppy:read\", \"poppy:write\"]";

fn signing(fake: &Fake) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    signing_on(fake, &host(), &dir, Arc::default(), BOTH)
}

fn ask(scopes: &[&str]) -> Ask {
    Ask {
        company: "shop".into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        new_account: false,
    }
}

/// `s` percent-encoded as a query value (RFC 3986's unreserved pass).
#[cfg(feature = "a2a")]
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Run the sign-in `ask` to its end as the runtime does, the scripted User
/// at the fake answering it: the gate's text, when one opened, and the
/// outcome. A poll still running after 30 s is cancelled, and the outcome
/// is that it never ended.
fn sign_in(a: &Agent, ask: &Ask) -> (Option<String>, Outcome) {
    match a.sup.begin_sign_in(ask, "signin-test", None) {
        Ok(Started::Held(o)) => (None, o),
        Ok(Started::Device { question, poll, .. }) => {
            let cancel = Arc::new(AtomicBool::new(false));
            let (tx, rx) = std::sync::mpsc::channel();
            let (sup, stop) = (Arc::clone(&a.sup), Arc::clone(&cancel));
            std::thread::spawn(move || {
                let _ = tx.send(sup.poll_device(poll, &stop));
            });
            let outcome = rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                    Outcome::Failed("the poll never ended".into())
                });
            (Some(question), outcome)
        }
        Ok(Started::Direct { question, .. }) => panic!("a Direct Sign-In: {question}"),
        Err(e) => panic!("{e}"),
    }
}

fn polls(record: &[Value]) -> Vec<&Value> {
    tokens(record)
        .into_iter()
        .filter(|t| t["grant"] == wire::grant::DEVICE_CODE)
        .collect()
}

fn refreshes(record: &[Value]) -> Vec<&Value> {
    tokens(record)
        .into_iter()
        .filter(|t| t["grant"] == wire::grant::REFRESH_TOKEN)
        .collect()
}

/// Every Account Token the fake issued, in order.
fn account_tokens(record: &[Value]) -> Vec<String> {
    record
        .iter()
        .filter_map(|r| r["issued"]["refresh_token"].as_str().map(str::to_string))
        .collect()
}

/// The `session_id` the agent's Bearer tokens were last asked for.
fn bearer_session(record: &[Value]) -> Value {
    tokens(record)
        .into_iter()
        .rfind(|t| t["resource"].is_string() && t["status"] == 200)
        .map(|t| t["session_id"].clone())
        .unwrap_or(Value::Null)
}

fn order(client: &::mcp::client::McpClient) -> Result<String, String> {
    match client.call_tool("order_status", Some(json!({"order_id": "o1"}))) {
        Ok(r) if !r.is_error() => Ok(r.text()),
        Ok(r) => Err(r.text()),
        Err(e) => Err(e.to_string()),
    }
}

/// Device Sign-In (draft §4.6), as the fake records it: the device
/// authorization asks for the scopes with the client assertion and a proof;
/// the gate's text is agentd's, naming the Company by its quoted name beside
/// its verified domain, with the link and the code to check; each poll comes
/// an interval after the last, for the current Session, with a proof; the
/// verification page is never opened by agentd; and once the User approves,
/// the Session is signed in — an account call answers.
#[test]
fn device_sign_in_keeps_the_interval_and_signs_the_session_in() {
    let f = fake(&[]);
    let a = signing(&f);
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    let sid = bearer_session(&f.record());
    assert!(sid.is_string());
    let (q, outcome) = sign_in(&a, &ask(&[]));
    let q = q.expect("a gate");
    let start = format!(
        "Sign in to \"Fake Company\" (127.0.0.1) for: view your account information, make changes. Open {}/auth/device/verify?user_code=",
        f.origin
    );
    assert!(q.starts_with(&start), "{q}");
    assert!(
        q.contains("and check that the page shows the code \"")
            && q.ends_with(
                "This closes by itself once you have signed in there; answer cancel to stop."
            ),
        "{q}"
    );
    assert_eq!(
        outcome,
        Outcome::SignedIn {
            scope: strs(&["poppy:read", "poppy:write"]),
            requested: strs(&["poppy:read", "poppy:write"])
        }
    );
    let record = f.record();
    let issuer = format!("{}/auth", f.origin);
    let auth = record
        .iter()
        .find(|r| r["path"] == "/auth/device")
        .expect("a device authorization");
    assert_eq!(auth["status"], 200, "{auth}");
    assert_eq!(auth["scope"], "poppy:read poppy:write", "{auth}");
    assert_eq!(auth["client_assertion"]["aud"], issuer.as_str());
    assert!(auth["proof"].is_object(), "a proof: {auth}");
    let p = polls(&record);
    assert_eq!(p.len(), 2, "one pending, one approved: {p:#?}");
    for poll in &p {
        assert_eq!(poll["session_id"], sid, "the current Session: {poll}");
        assert_eq!(poll["dpop"], true, "{poll}");
    }
    let at = |r: &Value| r["at_ms"].as_u64().unwrap();
    assert!(
        at(p[0]) >= at(auth) + 950,
        "the first poll an interval after"
    );
    assert!(
        at(p[1]) >= at(p[0]) + 950,
        "and the next an interval after it"
    );
    assert_eq!(p[0]["error"], wire::error::AUTHORIZATION_PENDING);
    assert_eq!(p[1]["status"], 200);
    assert!(
        record.iter().all(|r| r["path"] != "/auth/device/verify"),
        "agentd opened the User's page"
    );
    assert_eq!(a.sup.signed_in(&a.sup.company("shop").unwrap()), Some(true));
    // The Bearer tokens were the signed-out Session's: the next one is
    // asked for with the Account Token, and the account call answers.
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let record = f.record();
    let bearer = refreshes(&record)
        .into_iter()
        .find(|t| t["resource"].is_string())
        .expect("a Bearer token on the Account Token");
    assert_eq!(bearer["dpop"], false, "{bearer}");
    assert_eq!(bearer["session_id"], sid, "{bearer}");
    assert!(
        record.iter().all(|r| r["account_token_leaked"].is_null()),
        "{record:#?}"
    );
}

/// `slow_down` adds 5 s to the interval (RFC 8628 §3.5): the poll after it
/// comes six seconds later, and the fake saw no poll too early.
#[test]
fn slow_down_adds_five_seconds_to_the_interval() {
    let f = fake(&[]);
    let a = signing(&f);
    f.control(json!({"slow_down": 1, "approver": {"after_polls": 0}}));
    let (_, outcome) = sign_in(&a, &ask(&[]));
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    let record = f.record();
    let p = polls(&record);
    assert_eq!(p.len(), 2, "{p:#?}");
    assert_eq!(p[0]["error"], wire::error::SLOW_DOWN);
    let gap = p[1]["at_ms"].as_u64().unwrap() - p[0]["at_ms"].as_u64().unwrap();
    assert!(gap >= 5950, "the interval and 5 s more: {gap} ms");
    assert_eq!(p[1]["status"], 200);
}

/// The poll stops on `access_denied` and on `expired_token` (§4.6: "The
/// Personal Agent stops asking after either"), and the sign-in says which.
#[test]
fn access_denied_and_expired_token_end_the_poll() {
    let f = fake(&[]);
    let a = signing(&f);
    for (decision, want) in [("deny", Outcome::Denied), ("expire", Outcome::Expired)] {
        f.control(json!({"approver": {"decision": decision, "after_polls": 0}}));
        let before = polls(&f.record()).len();
        let (_, outcome) = sign_in(&a, &ask(&[]));
        assert_eq!(outcome, want);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            polls(&f.record()).len(),
            before + 1,
            "{decision}: no poll after it"
        );
    }
    assert!(!a.sup.holds_account(&a.sup.company("shop").unwrap()));
}

/// The User may approve fewer scopes than asked for (§4.4: "the Personal
/// Agent checks `scope` before continuing"): both are reported.
#[test]
fn a_narrower_grant_is_reported() {
    let f = fake(&[]);
    let a = signing(&f);
    f.control(json!({"approver": {"scope": "poppy:read"}}));
    let (_, outcome) = sign_in(&a, &ask(&[]));
    assert_eq!(
        outcome,
        Outcome::SignedIn {
            scope: strs(&["poppy:read"]),
            requested: strs(&["poppy:read", "poppy:write"])
        }
    );
    assert!(
        outcome
            .said("shop")
            .contains("fewer scopes than asked for: poppy:read of poppy:read poppy:write"),
        "{}",
        outcome.said("shop")
    );
}

/// A device authorization naming a `verification_uri` off the entry's hosts
/// is refused before any gate opens, naming the field and the host but
/// never the URL, whose query may carry the code — and nothing is polled.
#[test]
fn an_off_host_verification_uri_opens_no_gate() {
    let f = fake(&[Variant::OffHostVerificationUri]);
    let a = signing(&f);
    let e = match a.sup.begin_sign_in(&ask(&[]), "t", None) {
        Err(e) => e,
        Ok(_) => panic!("a gate would have opened"),
    };
    assert!(
        e.contains("the device authorization's verification_uri"),
        "{e}"
    );
    assert!(e.contains("names host localhost"), "{e}");
    assert!(
        !e.contains("/device/verify") && !e.contains("user_code"),
        "{e}"
    );
    assert!(polls(&f.record()).is_empty());
}

/// An Account Token held that covers the scopes signs the Session in with
/// the `refresh_token` grant and its `session_id`, and no gate opens
/// (§4.4: "SHOULD check whether it already holds an Account Token that
/// covers the task").
#[test]
fn a_held_account_token_that_covers_the_scopes_opens_no_gate() {
    let f = fake(&[]);
    let a = signing(&f);
    sign_in(&a, &ask(&[]));
    let before = f.record().len();
    let (q, outcome) = sign_in(&a, &ask(&["poppy:read"]));
    assert!(q.is_none(), "{q:?}");
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    let after = f.record()[before..].to_vec();
    assert!(
        after.iter().all(|r| r["path"] != "/auth/device"),
        "{after:#?}"
    );
    let r = refreshes(&after);
    assert_eq!(r.len(), 1, "{after:#?}");
    assert!(r[0]["session_id"].is_string() && r[0]["dpop"] == true);
}

/// A sign-in that asks for more than the Account Token grants asks for every
/// scope it needs, the held ones too (§4.4), and once it succeeds the token
/// it replaced is revoked (§4.4: "which the Personal Agent SHOULD revoke").
#[test]
fn a_scope_upgrade_revokes_the_old_account_token() {
    let f = fake(&[]);
    let a = signing(&f);
    f.control(json!({"approver": {"scope": "poppy:read"}}));
    sign_in(&a, &ask(&["poppy:read"]));
    let first = account_tokens(&f.record())[0].clone();
    f.control(json!({"approver": {"scope": "poppy:read poppy:write"}}));
    let (q, outcome) = sign_in(&a, &ask(&["poppy:write"]));
    assert!(q.is_some(), "a sign-in, not the held token");
    assert_eq!(
        outcome,
        Outcome::SignedIn {
            scope: strs(&["poppy:read", "poppy:write"]),
            requested: strs(&["poppy:read", "poppy:write"])
        }
    );
    let record = f.record();
    let auths: Vec<&Value> = record
        .iter()
        .filter(|r| r["path"] == "/auth/device")
        .collect();
    assert_eq!(auths[1]["scope"], "poppy:read poppy:write");
    let revoked: Vec<&Value> = record
        .iter()
        .filter(|r| r["path"] == "/auth/revoke")
        .collect();
    assert_eq!(revoked.len(), 1, "{record:#?}");
    assert_eq!(revoked[0]["account_token"], presented(&first));
    assert_eq!(revoked[0]["token_type_hint"], "refresh_token");
    assert_eq!(revoked[0]["revoked"], true);
}

/// Signing in to another account than the Session's (§4.4) issues no tokens
/// and leaves the held Account Token as it was; `new_account` signs in in a
/// new Session, and revokes the old token once it succeeds.
#[test]
fn account_mismatch_changes_nothing_and_a_new_account_starts_a_new_session() {
    let f = fake(&[]);
    let a = signing(&f);
    f.control(json!({"approver": {"scope": "poppy:read"}}));
    sign_in(&a, &ask(&["poppy:read"]));
    let first = account_tokens(&f.record())[0].clone();
    let sid = polls(&f.record())[0]["session_id"].clone();
    f.control(json!({"approver": {"account": "acct_2", "scope": "poppy:read poppy:write"}}));
    let (_, outcome) = sign_in(&a, &ask(&["poppy:write"]));
    assert_eq!(outcome, Outcome::AccountMismatch);
    assert_eq!(
        account_tokens(&f.record()),
        std::slice::from_ref(&first),
        "no tokens issued"
    );
    let company = a.sup.company("shop").unwrap();
    assert!(a.sup.holds_account(&company), "the held token is kept");
    let switch = Ask {
        new_account: true,
        ..ask(&["poppy:write"])
    };
    let (_, outcome) = sign_in(&a, &switch);
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    let record = f.record();
    let last = polls(&record).last().cloned().unwrap().clone();
    assert_ne!(last["session_id"], sid, "a new Session: {last}");
    let revoked: Vec<&Value> = record
        .iter()
        .filter(|r| r["path"] == "/auth/revoke")
        .collect();
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0]["account_token"], presented(&first));
}

/// `403 sign_in_required` with an Account Token held — a Session the
/// Company signed out — refreshes into the current Session, with
/// `session_id` and `resource` and no proof, and the call succeeds.
#[test]
fn sign_in_required_refreshes_into_the_current_session() {
    let f = fake(&[]);
    let a = signing(&f);
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    sign_in(&a, &ask(&[]));
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let sid = bearer_session(&f.record());
    f.control(json!({"sign_out_sessions": true}));
    let before = f.record().len();
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let record = f.record();
    let after = &record[before..];
    assert!(
        after
            .iter()
            .any(|r| r["path"] == "/mcp" && r["error"] == wire::error::SIGN_IN_REQUIRED),
        "{after:#?}"
    );
    let r = refreshes(after);
    assert_eq!(r.len(), 1, "{after:#?}");
    assert_eq!(r[0]["session_id"], sid, "the current Session");
    assert_eq!(r[0]["resource"], format!("{}/mcp", f.origin));
    assert_eq!(r[0]["dpop"], false);
    assert_eq!(r[0]["status"], 200);
}

/// With an Account Token held, an `invalid_token` from an expired Session
/// Token renews with it and keeps it, and an `invalid_session` starts a new
/// Session already signed in, with no `session_id` (§4.8) — only an
/// `invalid_grant` forgets the token, after which the Session goes on signed
/// out.
#[test]
fn only_invalid_grant_forgets_the_account_token() {
    let f = fake(&[]);
    let a = signing(&f);
    let company = a.sup.company("shop").unwrap();
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    sign_in(&a, &ask(&[]));
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    // invalid_token: renewed with the Account Token, which is kept.
    let before = f.record().len();
    f.control(json!({"refuse_tokens": 1}));
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let after = f.record()[before..].to_vec();
    assert!(
        after
            .iter()
            .any(|r| r["path"] == "/mcp" && r["error"] == wire::error::INVALID_TOKEN)
    );
    assert!(
        refreshes(&after).iter().all(|t| t["status"] == 200),
        "{after:#?}"
    );
    assert!(a.sup.holds_account(&company));
    // invalid_session: a new Session, signed in by the Account Token alone.
    let before = f.record().len();
    f.control(json!({"end_sessions": true, "refuse_tokens": 1}));
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let after = f.record()[before..].to_vec();
    let r = refreshes(&after);
    assert!(
        r.iter().any(|t| t["error"] == wire::error::INVALID_SESSION),
        "{after:#?}"
    );
    let started = r
        .iter()
        .find(|t| t["session_id"].is_null())
        .expect("a Session started on the Account Token");
    assert_eq!(started["status"], 200, "{started}");
    assert!(
        tokens(&after)
            .iter()
            .all(|t| t["grant"] != wire::grant::JWT_BEARER),
        "never signed out: {after:#?}"
    );
    assert!(a.sup.holds_account(&company));
    // invalid_grant — the User disconnected agentd at the Company: the
    // token is forgotten, and the Session goes on signed out.
    f.control(json!({"disconnect": true}));
    let before = f.record().len();
    let e = order(&client).expect_err("signed out");
    assert!(e.contains(wire::error::SIGN_IN_REQUIRED), "{e}");
    assert!(!a.sup.holds_account(&company));
    let after = f.record()[before..].to_vec();
    assert!(
        refreshes(&after)
            .iter()
            .any(|t| t["error"] == wire::error::INVALID_GRANT),
        "{after:#?}"
    );
    assert!(
        tokens(&after)
            .iter()
            .any(|t| t["grant"] == wire::grant::JWT_BEARER && t["status"] == 200),
        "{after:#?}"
    );
    let r = client
        .call_tool("search", Some(json!({"query": "boots"})))
        .unwrap();
    assert!(!r.is_error(), "a signed-out call still answers");
}

/// Signing out revokes the Account Token (§4.9, RFC 7009) — the fake records
/// it — and forgets it; the same Session goes on signed out.
#[test]
fn signing_out_revokes_and_the_session_goes_on_signed_out() {
    let f = fake(&[]);
    let a = signing(&f);
    let company = a.sup.company("shop").unwrap();
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    sign_in(&a, &ask(&[]));
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let token = account_tokens(&f.record())[0].clone();
    let sid = bearer_session(&f.record());
    let out = a.sup.sign_out(&company).unwrap();
    assert_eq!(
        out,
        json!({"company": "shop", "signed_out": true, "revoked": true})
    );
    let record = f.record();
    let revoke = record
        .iter()
        .find(|r| r["path"] == "/auth/revoke")
        .expect("revoked at the Company");
    assert_eq!(revoke["account_token"], presented(&token));
    assert_eq!(revoke["token_type_hint"], "refresh_token");
    assert_eq!(revoke["revoked"], true);
    assert!(revoke["client_assertion"].is_object());
    assert!(!a.sup.holds_account(&company));
    let e = order(&client).expect_err("signed out");
    assert!(e.contains(wire::error::SIGN_IN_REQUIRED), "{e}");
    assert_eq!(bearer_session(&f.record()), sid, "the same Session");
    assert_eq!(
        a.sup.sign_out(&company).unwrap()["signed_out"],
        false,
        "nobody is signed in"
    );
}

/// An Account Token the Company rotates is kept before it is used, sealed,
/// and a restart uses the newest: the fake never sees a spent one. No
/// Account Token is ever in the store in plaintext.
#[test]
fn a_rotated_account_token_survives_a_restart_and_is_kept_sealed() {
    let f = fake(&[Variant::Rotation]);
    let h = host();
    let dir = tempfile::tempdir().unwrap().keep();
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let first = signing_on(&f, &h, &dir, Arc::clone(&store), BOTH);
    sign_in(&first, &ask(&[]));
    let (client, _) =
        agentd::companies::mcp::connect(&first.sup, "shop", Duration::from_secs(20)).unwrap();
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let issued = account_tokens(&f.record());
    assert!(issued.len() >= 2, "a sign-in and a rotation: {issued:?}");
    drop((client, first));
    let before = f.record().len();
    let second = signing_on(&f, &h, &dir, Arc::clone(&store), BOTH);
    let (client, _) =
        agentd::companies::mcp::connect(&second.sup, "shop", Duration::from_secs(20)).unwrap();
    assert_eq!(order(&client).as_deref(), Ok("shipped"));
    let record = f.record();
    let after = refreshes(&record[before..]);
    assert_eq!(
        after[0]["account_token"],
        presented(issued.last().unwrap()),
        "the newest token, after the restart"
    );
    assert!(
        refreshes(&record).iter().all(|t| t["status"] == 200),
        "a spent token was presented: {record:#?}"
    );
    use agentd::store::Store;
    let dump: String = store
        .dump()
        .into_iter()
        .map(|(k, _)| format!("{k} {:?}", store.get(&k, None)))
        .collect();
    for t in account_tokens(&record) {
        assert!(!dump.contains(&t), "an Account Token in the store");
    }
}

/// A token granted on an Account Token holding more scopes than the entry
/// now allows asks for fewer (§4.8: "`scope` MAY ask for fewer scopes than
/// the Account Token has").
#[test]
fn an_account_token_is_used_for_no_more_than_the_entry_allows() {
    let f = fake(&[]);
    let h = host();
    let dir = tempfile::tempdir().unwrap().keep();
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let first = signing_on(&f, &h, &dir, Arc::clone(&store), BOTH);
    sign_in(&first, &ask(&[]));
    drop(first);
    let before = f.record().len();
    let narrowed = signing_on(&f, &h, &dir, store, "[\"poppy:read\"]");
    agentd::companies::mcp::connect(&narrowed.sup, "shop", Duration::from_secs(20)).unwrap();
    let record = f.record();
    let r = refreshes(&record[before..]);
    assert!(!r.is_empty());
    for t in r {
        assert_eq!(t["scope"], "poppy:read", "{t}");
        assert_eq!(t["status"], 200, "{t}");
    }
}

/// A held Account Token is not used where the entry admits none of its
/// scopes, nor at all once the entry signs nobody in: every request there
/// is signed out — no `refresh_token` grant is sent — and the token is kept
/// for the User to sign out of.
#[test]
fn an_account_token_the_entry_admits_none_of_is_not_used() {
    let f = fake(&[]);
    let h = host();
    let dir = tempfile::tempdir().unwrap().keep();
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let first = signing_on(&f, &h, &dir, Arc::clone(&store), BOTH);
    sign_in(&first, &ask(&[]));
    drop(first);
    for (kinds, scopes) in [("", ""), ("[device]", "[\"addresses\"]")] {
        let before = f.record().len();
        let settings = signing_config(&h, &f, &dir, kinds, scopes, "", "");
        let a = Agent {
            sup: supervisor_of(&settings, Arc::clone(&store), &h),
            _dir: dir.clone(),
        };
        let (client, _) =
            agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
        let e = order(&client).expect_err("signed out");
        assert!(e.contains(wire::error::SIGN_IN_REQUIRED), "{kinds}: {e}");
        let record = f.record();
        let after = &record[before..];
        assert!(refreshes(after).is_empty(), "{kinds}: {after:#?}");
        let company = a.sup.company("shop").unwrap();
        assert!(a.sup.holds_account(&company), "{kinds}: kept");
    }
}

/// A Company's `expires_in` and `interval` are only ever added to a clock:
/// a device authorization naming the largest numbers JSON carries is
/// followed for `DEVICE_FOR` at most, and its poll ends as asked rather
/// than overflow.
#[test]
fn a_device_authorizations_numbers_never_overflow_the_clock() {
    let f = fake(&[]);
    let a = signing(&f);
    f.control(json!({"device": {"expires_in": u64::MAX, "interval": u64::MAX}}));
    let poll = match a.sup.begin_sign_in(&ask(&[]), "t", None) {
        Ok(Started::Device { poll, .. }) => poll,
        Ok(_) => panic!("a Device Sign-In"),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(poll.expires_in(), agentd::companies::token::DEVICE_FOR);
    let cancel = AtomicBool::new(true);
    assert_eq!(a.sup.poll_device(poll, &cancel), Outcome::Cancelled);
}

/// Start renewing `a`'s Session at `company` with its Account Token, the
/// fake holding back its answer once the token is rotated: the renewal
/// holds the Session until then. Returns once the hold has begun.
fn renewing_slowly(
    f: &Fake,
    a: &Agent,
    company: &Arc<agentd::companies::Company>,
) -> std::thread::JoinHandle<Result<(bool, Vec<String>), String>> {
    // The fake's own line for a hold, which `Fake::record` leaves out.
    let holds = || {
        get(&format!("{}/_mock/record", f.origin)).1["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["path"] == "/_mock/held")
            .count()
    };
    let before = holds();
    f.control(json!({"stall": {"method": "token:refresh_token", "times": 1, "ms": 1500}}));
    let (sup, c) = (Arc::clone(&a.sup), Arc::clone(company));
    let t = std::thread::spawn(move || sup.with_session(&c, |s, env| s.renew_now(env)));
    let deadline = Instant::now() + Duration::from_secs(20);
    while holds() == before {
        assert!(
            Instant::now() < deadline,
            "the renewal never reached the fake"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    t
}

/// A sign-in that ends while a renewal is in flight is ordered after it by
/// the Session: the renewal's rotated Account Token is kept first, and it
/// is the one the sign-in's new token replaces — never written over the new
/// one after.
#[test]
fn a_renewal_in_flight_never_undoes_a_sign_in() {
    let f = fake(&[Variant::Rotation]);
    let a = signing(&f);
    let company = a.sup.company("shop").unwrap();
    sign_in(&a, &ask(&[]));
    let sid = a
        .sup
        .with_session(&company, |s, env| s.session_id(env))
        .unwrap();
    let renewal = renewing_slowly(&f, &a, &company);
    // The User finished signing in again meanwhile.
    let got = agentd::companies::token::TokenResponse {
        access_token: "at_signed_in".into(),
        token_type: "DPoP".into(),
        expires_in: Duration::from_secs(600),
        scope: strs(&["poppy:read", "poppy:write"]),
        session_id: sid,
        signed_in: true,
        refresh_token: Some("pat_from_the_new_sign_in".into()),
        refresh_token_expires_in: None,
    };
    let replaced = a
        .sup
        .with_session(&company, |s, env| s.signed_in(env, &got, "device"))
        .unwrap();
    renewal.join().unwrap().unwrap();
    let held = a
        .sup
        .with_session(&company, |s, env| env.account(&s.issuer))
        .unwrap()
        .unwrap();
    assert_eq!(
        held.token, "pat_from_the_new_sign_in",
        "the sign-in's token is kept"
    );
    assert_eq!(
        replaced.map(|r| r.token),
        account_tokens(&f.record()).last().cloned(),
        "the rotated token is the one replaced"
    );
}

/// Signing out while a renewal is in flight revokes the Account Token the
/// renewal rotated to, not the spent one it started from: none is left good
/// at the Company.
#[test]
fn signing_out_during_a_renewal_revokes_the_rotated_token() {
    let f = fake(&[Variant::Rotation]);
    let a = signing(&f);
    let company = a.sup.company("shop").unwrap();
    sign_in(&a, &ask(&[]));
    let renewal = renewing_slowly(&f, &a, &company);
    let out = a.sup.sign_out(&company).unwrap();
    renewal.join().unwrap().unwrap();
    assert_eq!(
        out,
        json!({"company": "shop", "signed_out": true, "revoked": true})
    );
    let record = f.record();
    let newest = account_tokens(&record).last().cloned().unwrap();
    let revoked: Vec<&Value> = record
        .iter()
        .filter(|r| r["path"] == "/auth/revoke")
        .collect();
    assert_eq!(revoked.len(), 1, "{record:#?}");
    assert_eq!(revoked[0]["account_token"], presented(&newest));
    assert_eq!(revoked[0]["revoked"], true);
    assert!(!a.sup.holds_account(&company));
}

/// Direct Sign-In (§4.5) through the fake, the test standing in for the
/// User's browser: the authorization URL carries S256 PKCE, the `state` and
/// the `redirect_uri` the client metadata lists; the callback's `state` is
/// spent on first use; and a response whose `iss` is not the Company's
/// issuer is refused, its code never exchanged.
#[cfg(feature = "a2a")]
#[test]
fn direct_sign_in_spends_its_state_once_and_refuses_another_issuer() {
    let f = fake(&[]);
    let h = host();
    let dir = tempfile::tempdir().unwrap().keep();
    let top = format!(
        "a2a:\n  listen: https://127.0.0.1:1\n  url: {}\n  tls: {{cert: {}, key: {}}}\n",
        h.origin,
        fixture("server.pem").display(),
        fixture("server.key").display(),
    );
    let settings = signing_config(&h, &f, &dir, "[direct]", BOTH, ", serve: true", &top);
    let a = Agent {
        sup: supervisor_of(&settings, Arc::default(), &h),
        _dir: dir.clone(),
    };
    let redirect = format!("{}/companies/callback", h.origin);
    assert_eq!(
        a.sup.identity.metadata()["redirect_uris"],
        json!([redirect.clone()])
    );
    // The link the gate shows, followed as the User's browser would.
    let begin = |a: &Agent, ask: Ask| -> (String, String) {
        let question = match a.sup.begin_sign_in(&ask, "direct-t", None) {
            Ok(Started::Direct { question, .. }) => question,
            Err(e) => panic!("a Direct Sign-In: {e}"),
            Ok(Started::Held(o)) => panic!("a Direct Sign-In, not held: {o:?}"),
            Ok(Started::Device { question, .. }) => panic!("a Direct Sign-In, not: {question}"),
        };
        assert!(
            question.starts_with("Sign in to \"Fake Company\" (127.0.0.1) for: view your account information, make changes. Open this link and sign in on the Company's own page: "),
            "{question}"
        );
        let url = question
            .split("page: ")
            .nth(1)
            .and_then(|r| r.split(" — ").next())
            .unwrap()
            .to_string();
        let r = agentd::auth::http::get(
            &url,
            &[],
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
        assert_eq!(r.status, 302, "the scripted User answers at once");
        let back = r.header("location").unwrap().to_string();
        assert!(back.starts_with(&format!("{redirect}?")), "{back}");
        (url, back.split_once('?').unwrap().1.to_string())
    };
    let (url, query) = begin(&a, ask(&[]));
    for part in [
        "response_type=code",
        "code_challenge_method=S256",
        &format!("redirect_uri={}", pct(&redirect)),
    ] {
        assert!(url.contains(part), "{part} in {url}");
    }
    let (id, outcome) = a.sup.direct_callback(&query).expect("its sign-in");
    assert_eq!(id, "direct-t");
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    assert!(
        a.sup.direct_callback(&query).is_none(),
        "the state is spent"
    );
    let codes = |r: &[Value]| -> Vec<Value> {
        tokens(r)
            .into_iter()
            .filter(|t| t["grant"] == wire::grant::AUTHORIZATION_CODE)
            .cloned()
            .collect()
    };
    let exchanged = codes(&f.record());
    assert_eq!(exchanged.len(), 1, "one exchange for one state");
    assert_eq!(
        exchanged[0]["status"], 200,
        "the same redirect_uri: {:?}",
        exchanged[0]
    );
    assert!(exchanged[0]["session_id"].is_string() && exchanged[0]["dpop"] == true);
    // Another issuer's response is refused, and its code is not exchanged.
    let another = Ask {
        new_account: true,
        ..ask(&[])
    };
    let (_, query) = begin(&a, another);
    let forged = query.replace(
        &pct(&format!("{}/auth", f.origin)),
        &pct("https://127.0.0.1:1/auth"),
    );
    assert_ne!(forged, query);
    let (_, outcome) = a.sup.direct_callback(&forged).expect("its sign-in");
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(why.contains("did not come from shop's issuer"), "{why}");
    assert!(a.sup.direct_callback(&query).is_none(), "spent with it");
    assert_eq!(
        codes(&f.record()).len(),
        1,
        "the forged code was not exchanged"
    );
}

// ---- Conversations and OpenAPI (RFC 0047 P3) ------------------------------

use agentd::companies::api::Call;
use agentd::companies::conversation::{Delivery, Item, NOT_AVAILABLE, Say};
use agentd::companies::request::CallError;

/// The entry fields of a Company used through its agent and its APIs.
const TALKS: &str = ", interfaces: [agent, openapi], api_allow: [\"GET /orders/{order_id}\", \"POST /orders/{order_id}/exchanges\", \"POST /notes\"]";

fn talking(fake: &Fake) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    agent_on(fake, &host(), &dir, Arc::default(), TALKS)
}

/// [`signing`], used through its agent and its APIs.
fn talking_signed(fake: &Fake) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    signing_on(
        fake,
        &host(),
        &dir,
        Arc::default(),
        &format!("{BOTH}{TALKS}"),
    )
}

/// What the readers of `a`'s conversations hand the stream, each delivery
/// acknowledged as the runtime acknowledges it.
fn stream_of(a: &Agent) -> Arc<Mutex<Vec<Item>>> {
    let got: Arc<Mutex<Vec<Item>>> = Arc::default();
    let into = Arc::clone(&got);
    a.sup.deliver_to(Arc::new(move |d: Delivery| {
        into.lock().unwrap().extend(d.items);
        let _ = d.ack.try_send(Ok(()));
    }));
    got
}

fn say(conversation: Option<&str>, text: &str, key: &str) -> Say {
    Say {
        conversation: conversation.map(str::to_string),
        text: Some(text.to_string()),
        key: key.to_string(),
        ..Say::default()
    }
}

fn shop(a: &Agent) -> Arc<agentd::companies::Company> {
    a.sup.company("shop").unwrap()
}

/// Wait up to 20 s for `done`, or fail saying `what`.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The conversation requests that reached the fake.
fn conversation_requests(f: &Fake) -> Vec<Value> {
    f.record()
        .into_iter()
        .filter(|r| r["conversation_request"] == true)
        .collect()
}

fn path_of(r: &Value) -> &str {
    r["path"].as_str().unwrap_or_default()
}

/// A message sent again after its answer was lost carries the same id, and
/// the Company answers it as the message it has; the same call sent with
/// other content is `message_id_conflict`. Every message agentd sends says
/// `sender: agent` and no `role`, holds to the kit's schema, and carries a
/// DPoP proof over the Session Token.
#[test]
fn a_retried_message_gets_the_first_answer_and_other_content_a_conflict() {
    let f = fake(&[]);
    let a = talking(&f);
    let first = a
        .sup
        .converse(&shop(&a), &say(None, "hello", "run/a#1"))
        .unwrap();
    let cid = first["conversation"].as_str().unwrap().to_string();
    assert!(cid.starts_with("cnv_"), "{first}");
    // The next message's answer is lost after the Company took it.
    f.control(json!({"http_fail": {"method": "POST", "path": format!("/poppy/conversations/{cid}/messages"), "times": 1, "status": 502}}));
    let second = a
        .sup
        .converse(&shop(&a), &say(Some(&cid), "second", "run/a#2"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second["conversation"], cid.as_str());
    let posts: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["method"] == "POST" && path_of(r).ends_with("/messages"))
        .collect();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[0]["failed"], 502);
    assert_eq!(
        posts[0]["message_id"], posts[1]["message_id"],
        "the same id"
    );
    assert_eq!(
        posts[1]["duplicate"], true,
        "answered as the message it has"
    );
    // The same call with other content: the id it was given, and the
    // Company's conflict, typed.
    let e = a
        .sup
        .converse(&shop(&a), &say(Some(&cid), "changed", "run/a#2"))
        .unwrap_err();
    assert!(e.is(wire::error::MESSAGE_ID_CONFLICT), "{e:?}");
    assert!(
        e.to_string()
            .contains("accepted a message with this id before, with other content"),
        "{e}"
    );
    for r in conversation_requests(&f)
        .iter()
        .filter(|r| r["method"] == "POST")
    {
        assert_eq!(r["sender"], "agent", "{r}");
        assert!(r["role"].is_null(), "role is never sent: {r}");
        assert!(
            r["validated"]
                .as_array()
                .unwrap()
                .contains(&json!("conversation-request.schema.json")),
            "{r}"
        );
        assert!(r["proof"]["ath"].is_string(), "{r}");
    }
}

/// The reader of an open conversation asks for a stream, skips the event
/// type the draft does not define, hands on no `text-delta` piece — to the
/// stream or to the model — and, as the stream ends, opens it again with
/// `Last-Event-ID` set to the last event it handled.
#[test]
fn the_reader_skips_unknown_events_hands_on_no_delta_and_reconnects_after_the_last() {
    let f = fake(&[]);
    let a = talking(&f);
    let got = stream_of(&a);
    let first = a
        .sup
        .converse(&shop(&a), &say(None, "boots in size 9", "k#1"))
        .unwrap();
    let cid = first["conversation"].as_str().unwrap().to_string();
    until("the Company's reply on the stream", || {
        got.lock()
            .unwrap()
            .iter()
            .any(|i| i.data.to_string().contains("You said: boots in size 9"))
    });
    let last = got.lock().unwrap().last().unwrap().id.clone();
    let last = last.split('/').nth(1).unwrap().to_string();
    until("a stream opened again after the last event", || {
        conversation_requests(&f)
            .iter()
            .any(|r| r["stream"] == true && r["last_event_id"] == last.as_str())
    });
    let items = got.lock().unwrap().clone();
    let all: String = items.iter().map(|i| i.data.to_string()).collect();
    assert!(
        !all.contains("DELTA:"),
        "no piece reaches the stream: {all}"
    );
    assert!(
        !all.contains("x-mock-receipt"),
        "the unknown type is skipped: {all}"
    );
    for i in &items {
        assert_eq!(i.data["conversation"], cid.as_str());
        assert!(
            ["message", "state", "authorization", "user_requested"].contains(&i.subject.as_str()),
            "{i:?}"
        );
    }
    let reads: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| path_of(r).ends_with("/events"))
        .collect();
    assert!(
        reads
            .iter()
            .any(|r| r["stream"] == true && r["last_event_id"].is_null()),
        "{reads:#?}"
    );
    // The model's read, as JSON, is held to the same rules.
    let read = a.sup.read(&shop(&a), &cid, 0, None).unwrap();
    let said = read["events"].to_string();
    assert!(
        said.contains("You said: boots in size 9")
            && !said.contains("x-mock-receipt")
            && !said.contains("DELTA:"),
        "{read}"
    );
    assert_eq!(read["status"], "idle");
    // Read again, it has nothing new.
    assert_eq!(
        a.sup.read(&shop(&a), &cid, 0, None).unwrap()["events"],
        json!([])
    );
}

/// After a restart the reader resumes from the cursor it saved: its first
/// read names it, with no `Last-Event-ID`, and nothing the first life
/// handed on is handed on again.
#[test]
fn a_restart_resumes_from_the_saved_cursor() {
    let f = fake(&[]);
    let (h, dir) = (host(), tempfile::tempdir().unwrap().keep());
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let a = agent_on(&f, &h, &dir, Arc::clone(&store), TALKS);
    let got = stream_of(&a);
    let cid = a
        .sup
        .converse(&shop(&a), &say(None, "hello", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    until("the reply on the stream", || {
        got.lock()
            .unwrap()
            .iter()
            .any(|i| i.data.to_string().contains("You said: hello"))
    });
    drop(a);
    // The stopped reader's read in flight ends within the fake's hold.
    std::thread::sleep(Duration::from_millis(3500));
    let before = f.record().len();
    let last = got
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .id
        .split('/')
        .nth(1)
        .unwrap()
        .to_string();
    let b = agent_on(&f, &h, &dir, store, TALKS);
    let again = stream_of(&b);
    b.sup.resume_conversations();
    until("the resumed reader's first read", || {
        f.record()[before..]
            .iter()
            .any(|r| r["conversation"] == cid.as_str() && path_of(r).ends_with("/events"))
    });
    let first = f.record()[before..]
        .iter()
        .find(|r| path_of(r).ends_with("/events"))
        .cloned()
        .unwrap();
    assert_eq!(first["cursor"], last.as_str(), "the saved cursor: {first}");
    assert!(first["last_event_id"].is_null(), "{first}");
    assert_eq!(b.sup.conversations_open(), 1);
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        again
            .lock()
            .unwrap()
            .iter()
            .all(|i| !i.data.to_string().contains("You said: hello"))
    );
}

/// A cursor the Company no longer keeps: the read starts again from the
/// beginning, and the model is told, once, that part of the conversation
/// is missing — the reader saying it on the stream too.
#[test]
fn cursor_expired_reads_again_from_the_start_and_says_part_is_missing() {
    let f = fake(&[]);
    let a = talking(&f);
    let got = stream_of(&a);
    let cid = a
        .sup
        .converse(&shop(&a), &say(None, "hello", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    a.sup.read(&shop(&a), &cid, 0, None).unwrap();
    f.control(json!({"expire_events": true}));
    a.sup
        .converse(&shop(&a), &say(Some(&cid), "after the gap", "k#2"))
        .unwrap();
    until("the reader saying it on the stream", || {
        got.lock()
            .unwrap()
            .iter()
            .any(|i| i.subject == "cursor_expired")
    });
    let read = a.sup.read(&shop(&a), &cid, 0, None).unwrap();
    assert!(
        read["notes"]
            .to_string()
            .contains("part of this conversation is missing"),
        "{read}"
    );
    assert!(
        read["events"]
            .to_string()
            .contains("You said: after the gap"),
        "{read}"
    );
    let reads: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| path_of(r).ends_with("/events") && r["stream"] == false)
        .collect();
    let expired = reads
        .iter()
        .position(|r| r["error"] == wire::error::CURSOR_EXPIRED)
        .expect("a cursor_expired");
    assert!(
        reads[expired + 1]["cursor"].is_null(),
        "read again from the start: {reads:#?}"
    );
    assert!(
        !a.sup.read(&shop(&a), &cid, 0, None).unwrap()["notes"]
            .to_string()
            .contains("missing"),
        "said once"
    );
}

/// An `authorization` event (§7.11) from a signed-out Session reaches the
/// model as a fact, saying company.sign_in asks the User; once the User
/// signed in, the same conversation goes on with the signed-in token. With
/// the Account Token held, a message the Company refuses `sign_in_required`
/// signs the Session in with it and is sent once more.
#[test]
fn an_authorization_event_continues_the_same_conversation_after_sign_in() {
    let f = fake(&[]);
    let a = talking_signed(&f);
    let mut ask_order = say(None, "Where is my order?", "k#1");
    ask_order.wait = Some(5);
    let first = a.sup.converse(&shop(&a), &ask_order).unwrap();
    let cid = first["conversation"].as_str().unwrap().to_string();
    assert!(
        first["events"].to_string().contains("\"authorization\""),
        "{first}"
    );
    let notes = first["notes"].to_string();
    assert!(
        notes.contains(
            "holds no Account Token that covers it: company.sign_in asks the User to sign in"
        ),
        "{notes}"
    );
    let (_, outcome) = sign_in(&a, &ask(&["poppy:read"]));
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    let mut again = say(Some(&cid), "Please check my order now.", "k#2");
    again.wait = Some(5);
    let next = a.sup.converse(&shop(&a), &again).unwrap();
    assert_eq!(next["conversation"], cid.as_str(), "the same conversation");
    assert!(
        next["events"]
            .to_string()
            .contains("Your order o1 has shipped."),
        "{next}"
    );
    let posts: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["method"] == "POST")
        .collect();
    assert_eq!(posts[0]["signed_in"], false, "{posts:#?}");
    let after = posts
        .iter()
        .find(|r| r["text"] == "Please check my order now.")
        .unwrap();
    assert_eq!(after["signed_in"], true, "the new token: {after}");
    assert!(path_of(after).contains(&cid));
    // The Company signs the Session out; the conversation belongs to the
    // account now, so its next message is `sign_in_required` — answered
    // with the Account Token, and sent once more.
    f.control(json!({"sign_out_sessions": true}));
    let third = a
        .sup
        .converse(&shop(&a), &say(Some(&cid), "Thanks for checking.", "k#3"));
    assert!(third.is_ok(), "{third:?}");
    let last: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| path_of(r).ends_with("/messages") && r["status"] != 202)
        .collect();
    assert!(
        last.iter()
            .any(|r| r["error"] == wire::error::SIGN_IN_REQUIRED),
        "{last:#?}"
    );
    let sent: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["text"] == "Thanks for checking.")
        .collect();
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0]["signed_in"], true, "{sent:#?}");
}

/// The Company asks to talk with the User directly (§7.10): in this phase
/// agentd says in the conversation that the User is not available, once,
/// as the agent.
#[test]
fn a_request_to_talk_with_the_user_is_declined_in_the_conversation() {
    let f = fake(&[]);
    let a = talking(&f);
    f.control(
        json!({"conversation": {"user_requested": "A specialist needs to confirm the exchange."}}),
    );
    let mut first = say(None, "I would like an exchange.", "k#1");
    first.wait = Some(5);
    let out = a.sup.converse(&shop(&a), &first).unwrap();
    assert!(
        out["events"].to_string().contains("\"user_requested\""),
        "{out}"
    );
    let notes = out["notes"].to_string();
    assert!(
        notes.contains("answered in the conversation that the User is not available"),
        "{notes}"
    );
    assert!(
        notes.contains("A specialist needs to confirm the exchange."),
        "quoted: {notes}"
    );
    until("the decline sent", || {
        conversation_requests(&f)
            .iter()
            .any(|r| r["text"] == NOT_AVAILABLE)
    });
    std::thread::sleep(Duration::from_millis(500));
    let declined: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["text"] == NOT_AVAILABLE)
        .collect();
    assert_eq!(declined.len(), 1, "once: {declined:#?}");
    assert_eq!(declined[0]["sender"], "agent");
    assert_eq!(declined[0]["status"], 202);
}

/// An operation the description does not have, one `api_allow` does not
/// list, and a body that fails the operation's schema are each refused
/// before anything is sent; what passes is sent with a DPoP proof over the
/// Session Token, a write with an `Idempotency-Key`.
#[test]
fn an_operation_outside_the_description_or_api_allow_or_its_schema_is_refused_unsent() {
    let f = fake(&[]);
    let a = talking(&f);
    let call = |method: &str, path: &str, json: Option<Value>| Call {
        method: method.into(),
        path: path.into(),
        json,
        key: format!("{method} {path}"),
        ..Call::default()
    };
    let e = a
        .sup
        .call_api(&shop(&a), &call("GET", "/nope", None))
        .unwrap_err();
    assert!(e.to_string().contains("has no operation GET /nope"), "{e}");
    let e = a
        .sup
        .call_api(&shop(&a), &call("GET", "/session", None))
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("GET /session is not in services.shop.api_allow"),
        "{e}"
    );
    let e = a
        .sup
        .call_api(
            &shop(&a),
            &call(
                "POST",
                "/orders/o1/exchanges",
                Some(json!({"item_id": 7, "replacement_sku": "x"})),
            ),
        )
        .unwrap_err();
    assert!(matches!(e, CallError::Refused(_)), "{e:?}");
    assert!(
        e.to_string().contains("the body:") && e.to_string().contains("nothing was sent"),
        "{e}"
    );
    // A path a server may resolve to an operation `api_allow` does not list.
    for path in ["/orders/../notes", "/orders/o1%2F..%2F..%2Fnotes/exchanges"] {
        let e = a
            .sup
            .call_api(&shop(&a), &call("POST", path, Some(json!({"text": "x"}))))
            .unwrap_err();
        assert!(matches!(e, CallError::Refused(_)), "{path}: {e:?}");
        assert!(e.to_string().contains("which a server may"), "{path}: {e}");
    }
    let api: Vec<Value> = f
        .record()
        .into_iter()
        .filter(|r| path_of(r).starts_with("/api/") && r["path"] != "/api/openapi.json")
        .collect();
    assert!(api.is_empty(), "nothing reached the API: {api:#?}");
    let ok = a
        .sup
        .call_api(
            &shop(&a),
            &call(
                "POST",
                "/notes",
                Some(json!({"text": "leave it at the door"})),
            ),
        )
        .unwrap();
    assert_eq!(ok["status"], 201, "{ok}");
    let note = f
        .record()
        .into_iter()
        .find(|r| r["path"] == "/api/notes")
        .unwrap();
    assert!(note["proof"]["ath"].is_string(), "{note}");
    assert!(
        note["idempotency_key"]
            .as_str()
            .is_some_and(|k| k.len() == 36),
        "a key on every write: {note}"
    );
}

/// A write whose operation declares `Idempotency-Key` is sent again with
/// the same key when its answer is lost, and gets the first answer; one
/// that declares none is `outcome unknown`, and is not sent again.
#[test]
fn an_idempotency_key_is_reused_on_retry_and_a_write_without_one_is_not_retried() {
    let f = fake(&[]);
    let a = talking(&f);
    f.control(json!({"http_fail": {"method": "POST", "path": "/api/orders/o1/exchanges", "times": 1, "status": 502}}));
    let ok = a
        .sup
        .call_api(
            &shop(&a),
            &Call {
                method: "POST".into(),
                path: "/orders/o1/exchanges".into(),
                json: Some(
                    json!({"item_id": "itm_4Qa", "replacement_sku": "stormline-insulated-m"}),
                ),
                key: "run/x#1".into(),
                ..Call::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ok["status"], 201, "{ok}");
    let sent: Vec<Value> = f
        .record()
        .into_iter()
        .filter(|r| r["path"] == "/api/orders/o1/exchanges")
        .collect();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(
        sent[0]["idempotency_key"], sent[1]["idempotency_key"],
        "the same key"
    );
    assert_eq!(sent[1]["duplicate"], true);
    assert_ne!(
        sent[0]["proof"]["jti"], sent[1]["proof"]["jti"],
        "a fresh proof"
    );
    f.control(
        json!({"http_fail": {"method": "POST", "path": "/api/notes", "times": 1, "status": 502}}),
    );
    let e = a
        .sup
        .call_api(
            &shop(&a),
            &Call {
                method: "POST".into(),
                path: "/notes".into(),
                json: Some(json!({"text": "hi"})),
                key: "run/x#2".into(),
                ..Call::default()
            },
        )
        .unwrap_err();
    assert!(matches!(e, CallError::Unknown(_)), "{e:?}");
    assert!(
        e.to_string()
            .starts_with("outcome unknown: company shop may have performed POST /notes"),
        "{e}"
    );
    assert_eq!(
        f.record()
            .iter()
            .filter(|r| r["path"] == "/api/notes")
            .count(),
        1,
        "sent once"
    );
}

/// `WWW-Authenticate` errors at an API are handled as at an MCP server
/// (§6): `invalid_token` renews and sends once more; `sign_in_required`
/// with no Account Token is typed; `insufficient_scope` names the scope.
#[test]
fn www_authenticate_errors_map_as_for_mcp() {
    let f = fake(&[]);
    let a = talking(&f);
    let get = |a: &Agent, path: &str| {
        a.sup.call_api(
            &shop(a),
            &Call {
                method: "GET".into(),
                path: path.into(),
                key: "k".into(),
                ..Call::default()
            },
        )
    };
    let e = get(&a, "/orders/o1").unwrap_err();
    assert!(e.is(wire::error::SIGN_IN_REQUIRED), "{e:?}");
    assert!(
        e.to_string().contains("company.sign_in asks the User"),
        "{e}"
    );
    f.control(json!({"refuse_dpop": 1}));
    let before = tokens(&f.record()).len();
    let e = get(&a, "/orders/o1").unwrap_err();
    assert!(
        e.is(wire::error::SIGN_IN_REQUIRED),
        "renewed, then the operation's own refusal: {e:?}"
    );
    let sent: Vec<Value> = f
        .record()
        .into_iter()
        .filter(|r| r["path"] == "/api/orders/o1")
        .collect();
    let n = sent.len();
    assert_eq!(
        sent[n - 2]["error"],
        wire::error::INVALID_TOKEN,
        "{sent:#?}"
    );
    assert_ne!(
        sent[n - 2]["presented"],
        sent[n - 1]["presented"],
        "a new token"
    );
    assert!(tokens(&f.record()).len() > before, "renewed");
    // Signed in without poppy:read: the scope the Company names, quoted.
    let s = talking_signed(&f);
    f.control(json!({"approver": {"scope": "poppy:write"}}));
    let (_, outcome) = sign_in(&s, &ask(&["poppy:write"]));
    assert!(matches!(outcome, Outcome::SignedIn { .. }), "{outcome:?}");
    let e = get(&s, "/orders/o1").unwrap_err();
    assert!(e.is(wire::error::INSUFFICIENT_SCOPE), "{e:?}");
    assert!(
        e.to_string().contains("it needs the scope \"poppy:read\""),
        "{e}"
    );
}

/// At most `limits.conversations` conversations are open, each with its
/// reader; closing one stops its reader and frees its place, and a closed
/// conversation takes no more messages.
#[test]
fn the_conversation_limit_holds_and_closing_frees_a_place() {
    let f = fake(&[]);
    let dir = tempfile::tempdir().unwrap().keep();
    let h = host();
    let mut settings = config(&h, &f, &dir, TALKS);
    settings.security.companies.limits.conversations = Some(1);
    let durable = Arc::new(agentd::state::Durable::new(
        Arc::<agentd::store::memory::MemoryStore>::default(),
        "t/",
        "t",
        agentd::state::Policy::default(),
        None,
    ));
    let a = Agent {
        sup: Supervisor::build(&settings, durable, &logger())
            .unwrap()
            .unwrap(),
        _dir: dir,
    };
    h.publish(&a.sup.identity);
    let one = a.sup.converse(&shop(&a), &say(None, "one", "k#1")).unwrap();
    let cid = one["conversation"].as_str().unwrap().to_string();
    let e = a
        .sup
        .converse(&shop(&a), &say(None, "two", "k#2"))
        .unwrap_err();
    assert!(matches!(e, CallError::Unsent(_)), "{e:?}");
    assert!(e.to_string().contains("1 conversations are open"), "{e}");
    let closed = a
        .sup
        .conversation_op(&shop(&a), &cid, "close", None)
        .unwrap();
    assert_eq!(closed["status"], "closed");
    assert_eq!(a.sup.conversations_open(), 0);
    let e = a
        .sup
        .converse(&shop(&a), &say(Some(&cid), "more", "k#3"))
        .unwrap_err();
    assert!(e.to_string().contains("is closed"), "{e}");
    a.sup.converse(&shop(&a), &say(None, "two", "k#2")).unwrap();
    assert_eq!(a.sup.conversations_open(), 1);
}

/// A read the Company is asked to hold for longer than the entry's
/// `timeout` is waited for as long as it was asked to: an idle conversation
/// answers no events, not a read that got no answer.
#[test]
fn a_held_read_outlasts_the_entrys_timeout() {
    let f = fake(&[]);
    let dir = tempfile::tempdir().unwrap().keep();
    let a = agent_on(
        &f,
        &host(),
        &dir,
        Arc::default(),
        &format!("{TALKS}, timeout: 1s"),
    );
    let cid = a
        .sup
        .converse(&shop(&a), &say(None, "hello", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    a.sup.read(&shop(&a), &cid, 0, None).unwrap();
    let begun = Instant::now();
    let read = a
        .sup
        .read(&shop(&a), &cid, 2, None)
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(read["events"], json!([]), "{read}");
    assert!(begun.elapsed() >= Duration::from_secs(2), "it was held");
    let waits: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| path_of(r).ends_with("/events") && r["stream"] == false)
        .collect();
    assert_eq!(waits.len(), 2, "one read each, none sent again: {waits:#?}");
}

/// A request made for a unit of work names, under the strict profile, the
/// trace the unit has at this Company — the same for every request of the
/// unit there, never the unit's own — and under `standard` the unit's own
/// trace. A reader's requests belong to no unit, and name none under
/// `strict`.
#[test]
fn a_request_names_the_trace_its_profile_gives() {
    let f = fake(&[]);
    let a = talking(&f);
    let (one, two) = (
        agentd::obs::trace::TraceContext::mint(),
        agentd::obs::trace::TraceContext::mint(),
    );
    let get = |a: &Agent, unit: &agentd::obs::trace::TraceContext| {
        let (sent, link) = a
            .sup
            .request_trace(&shop(a), Some(unit), Some(&unit.child()))
            .unwrap();
        let before = f.record().len();
        a.sup
            .call_api(
                &shop(a),
                &Call {
                    method: "POST".into(),
                    path: "/notes".into(),
                    json: Some(json!({"text": "traced"})),
                    key: format!("{}#{before}", unit.trace_id),
                    trace: sent,
                    ..Call::default()
                },
            )
            .unwrap();
        let r = f.record()[before..]
            .iter()
            .find(|r| r["path"] == "/api/notes")
            .cloned()
            .unwrap();
        (r["trace_header"].as_str().unwrap().to_string(), link)
    };
    let (first, link) = get(&a, &one);
    assert_ne!(first, one.trace_id, "never the unit's own trace");
    assert_eq!(
        link.map(|l| l.trace_id),
        Some(first.clone()),
        "agentd's span links to what was sent"
    );
    assert_eq!(get(&a, &one).0, first, "one trace per unit at the Company");
    assert_ne!(get(&a, &two).0, first, "another unit, another trace");
    // A reader's requests name none under the strict profile.
    let cid = a
        .sup
        .converse(&shop(&a), &say(None, "hello", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    until("the reader's first read", || {
        conversation_requests(&f)
            .iter()
            .any(|r| r["conversation"] == cid.as_str() && r["stream"] == true)
    });
    for r in conversation_requests(&f) {
        assert!(r["trace_header"].is_null(), "{r}");
    }
    let dir = tempfile::tempdir().unwrap().keep();
    let s = agent_on(
        &f,
        &host(),
        &dir,
        Arc::default(),
        &format!("{TALKS}, privacy: standard"),
    );
    let (sent, link) = get(&s, &one);
    assert_eq!(sent, one.trace_id, "the unit's own trace");
    assert!(link.is_none());
}

// ---- Operations and Direct Conversations (RFC 0047 P4) --------------------

use agentd::companies::ledger::{Approval, Channel};
use agentd::companies::operations::{Outcome as Went, Step};

/// The entry fields of a Company whose exchanges are operations: its MCP
/// server, its agent and the exchanges of its API.
const OPERATES: &str =
    ", interfaces: [mcp, agent, openapi], api_allow: [\"POST /orders/{order_id}/exchanges\"]";

/// A standing permission over the API's exchanges that cost at most $20.
#[cfg(feature = "cel")]
fn cheap_exchanges(uses: &str) -> String {
    format!(
        "      - {{company: shop, via: {{api: \"POST /orders/{{order_id}}/exchanges\"}}, terms: [order_id, item_id, replacement_sku, price_difference], when: \"terms.price_difference.currency == 'USD' && double(terms.price_difference.amount) <= 20.0\", expires: \"2099-01-01T00:00:00Z\"{uses}}}\n"
    )
}

/// An agent that lists the Operations extension at `fake`, with the
/// standing permissions `standing` (lines of a YAML list), on `store`.
fn operating_on(
    fake: &Fake,
    host: &Host,
    dir: &Path,
    store: Arc<agentd::store::memory::MemoryStore>,
    standing: &str,
) -> Agent {
    operating_with(fake, host, dir, store, standing)
}

/// [`operating_on`] on any store.
fn operating_with(
    fake: &Fake,
    host: &Host,
    dir: &Path,
    store: Arc<dyn agentd::store::Store>,
    standing: &str,
) -> Agent {
    let standing = if standing.is_empty() {
        String::new()
    } else {
        format!("    standing:\n{standing}")
    };
    let yaml = format!(
        "security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"{h}/companies/client.json\", name: \"Test agent\", key_file: \"{d}/client-key\", extensions: [operations]}}\n{standing}\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         services:\n  shop: {{kind: company, endpoint: \"{o}\", allow_private: true{OPERATES}}}\n",
        ca = fixture("ca.pem").display(),
        h = host.origin,
        d = dir.display(),
        o = fake.origin,
    );
    let path = dir.join("c.yaml");
    std::fs::write(&path, yaml).unwrap();
    let settings =
        agentd::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .unwrap_or_else(|e| panic!("{e}"))
            .0
            .settings;
    Agent {
        sup: supervisor_with(&settings, store, host),
        _dir: dir.to_path_buf(),
    }
}

fn operating(fake: &Fake, standing: &str) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    operating_on(fake, &host(), &dir, Arc::default(), standing)
}

/// Propose exchanging `item` for `sku` through the API, as call `key`: the
/// operation's id.
fn propose(a: &Agent, item: &str, sku: &str, key: &str) -> String {
    let out = a
        .sup
        .call_api(
            &shop(a),
            &Call {
                method: "POST".into(),
                path: "/orders/ord_7Hk2/exchanges".into(),
                json: Some(json!({"item_id": item, "replacement_sku": sku})),
                key: key.into(),
                ..Call::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(out["status"], 202, "{out}");
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .contains("is not done until confirmed"),
        "{out}"
    );
    out["json"]["operation"]["operation_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Confirm `id`, `given` the User's approval or none: every step, in order.
fn confirm(a: &Agent, id: &str, given: Option<Approval>) -> Vec<Step> {
    let steps = Mutex::new(Vec::new());
    a.sup
        .confirm_operation(&shop(a), id, given, None, &AtomicBool::new(false), &|s| {
            steps.lock().unwrap().push(s)
        });
    steps.into_inner().unwrap()
}

/// The User's approval of what `steps` asked them: its revision, and what
/// the gate showed of it.
fn approval(steps: &[Step]) -> Approval {
    steps
        .iter()
        .find_map(|s| match s {
            Step::Ask {
                revision, digest, ..
            } => Some(Approval {
                revision: *revision,
                digest: digest.clone(),
                approved_by: "user".into(),
                by: "operator".into(),
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("nobody was asked: {steps:?}"))
}

/// The User's approval of what a confirm of `id` asks them now.
fn approve(a: &Agent, id: &str) -> Approval {
    approval(&confirm(a, id, None))
}

/// The confirms that reached the fake.
fn confirms(f: &Fake) -> Vec<Value> {
    f.record()
        .into_iter()
        .filter(|r| r["operation_request"] == true && path_of(r).ends_with("/confirm"))
        .collect()
}

/// The question a confirm's steps asked the User, and its revision.
fn asked(steps: &[Step]) -> Option<(String, u64)> {
    steps.iter().find_map(|s| match s {
        Step::Ask {
            question, revision, ..
        } => Some((question.clone(), *revision)),
        _ => None,
    })
}

/// Whether a confirm's steps used a standing permission.
fn standing_used(steps: &[Step]) -> bool {
    steps.iter().any(|s| matches!(s, Step::Standing { .. }))
}

/// How a confirm's steps ended.
fn done(steps: &[Step]) -> Result<Value, String> {
    steps
        .iter()
        .find_map(|s| match s {
            Step::Done(r) => Some(r.clone()),
            _ => None,
        })
        .expect("it ended")
}

/// An operation is recognised where the Operations extension puts it — the
/// API's `202 {operation}` — and proposed only to an agent that lists the
/// extension; and no confirm is sent without the User's approval of its
/// revision: the first confirm asks them, in agentd's words with the
/// Company's summary and terms quoted beside its verified domain, and sends
/// nothing; the one they approved is sent with `approved_by: user` and that
/// revision, and a final operation is never confirmed again.
#[test]
fn no_confirm_is_sent_without_the_users_approval_of_its_revision() {
    let f = fake(&[]);
    // An agent that does not list the extension is never proposed one
    // (Operations §2): the exchange is made.
    let plain = talking(&f);
    let made = plain
        .sup
        .call_api(
            &shop(&plain),
            &Call {
                method: "POST".into(),
                path: "/orders/ord_7Hk2/exchanges".into(),
                json: Some(
                    json!({"item_id": "itm_4Qa", "replacement_sku": "stormline-insulated-m"}),
                ),
                key: "plain#1".into(),
                ..Call::default()
            },
        )
        .unwrap();
    assert_eq!(made["status"], 201, "{made}");
    let a = operating(&f, "");
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let steps = confirm(&a, &id, None);
    let (q, revision) = asked(&steps).unwrap_or_else(|| panic!("{steps:?}"));
    let approved = approval(&steps);
    assert_eq!(revision, 1);
    assert!(
        q.starts_with("\"Fake Company\" (127.0.0.1) asks you to approve this, in your signed-out Session at 127.0.0.1. The Company says: \"Exchange item itm_4Qa on order ord_7Hk2 for stormline-insulated-m, for $70.00 more"),
        "{q}"
    );
    assert!(q.contains("\"price_difference.amount\": \"70.00\""), "{q}");
    assert!(
        q.contains(&format!("This is revision 1 of operation {id}")),
        "{q}"
    );
    assert!(q.ends_with("Answer approve or decline."), "{q}");
    assert!(
        confirms(&f).is_empty(),
        "nothing confirmed before the User approved"
    );
    let steps = confirm(&a, &id, Some(approved.clone()));
    assert!(
        steps.contains(&Step::Confirmed {
            revision: 1,
            approved_by: "user".into(),
            outcome: Went::Taken,
            state: Some("succeeded".into()),
            error: None
        }),
        "{steps:?}"
    );
    let out = done(&steps).unwrap();
    assert_eq!(out["state"], "succeeded");
    assert_eq!(out["approved_by"], "user");
    assert!(out["operation"]["result"]["summary"].is_string(), "{out}");
    let sent = confirms(&f);
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0]["revision"], 1);
    assert_eq!(sent[0]["approved_by"], "user");
    assert!(
        sent[0]["proof"]["ath"].is_string(),
        "with the DPoP Session Token: {}",
        sent[0]
    );
    // Final: nothing is confirmed again.
    let again = done(&confirm(&a, &id, Some(approved))).unwrap();
    assert!(
        again["note"].as_str().unwrap().contains("is succeeded"),
        "{again}"
    );
    assert_eq!(confirms(&f).len(), 1);
}

/// `terms_changed` asks again (Ops §5.1: "An approval covers one
/// revision"): the User's approval of revision 1 does not cover revision 2,
/// and the question says what changed.
#[test]
fn terms_changed_asks_again() {
    let f = fake(&[]);
    let a = operating(&f, "");
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let first = approve(&a, &id);
    f.control(json!({"operations": {"revise": 1}}));
    let steps = confirm(&a, &id, Some(first.clone()));
    assert!(
        steps.contains(&Step::Confirmed {
            revision: 1,
            approved_by: "user".into(),
            outcome: Went::Refused,
            state: None,
            error: Some("terms_changed".into())
        }),
        "{steps:?}"
    );
    let second = approval(&steps);
    let (q, revision) = asked(&steps).unwrap_or_else(|| panic!("{steps:?}"));
    assert_eq!(revision, 2);
    assert!(
        q.contains(
            "Changed from the revision before: \"price_difference.amount\": \"70.00\" → \"85.00\""
        ),
        "{q}"
    );
    assert!(
        q.contains("The Company changed the terms since they were last approved."),
        "{q}"
    );
    // The old approval is gone: the User is asked again, nothing sent.
    assert!(asked(&confirm(&a, &id, None)).is_some());
    assert!(asked(&confirm(&a, &id, Some(first))).is_some());
    assert_eq!(confirms(&f).len(), 1);
    let ok = confirm(&a, &id, Some(second));
    assert_eq!(done(&ok).unwrap()["state"], "succeeded");
    let sent = confirms(&f);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["revision"], 2);
}

/// A standing permission confirms what it covers with `approved_by:
/// standing_permission` and asks no one; a revision it does not cover asks
/// the User: one dearer than its predicate allows, one with a term key it
/// does not list, one with no terms, and one the Company holds to the User's
/// own approval — which no standing permission ever confirms, even when the
/// Company says so only once the confirm arrives.
#[cfg(feature = "cel")]
#[test]
fn a_standing_permission_confirms_only_what_it_covers() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(""));
    let cheap = propose(&a, "itm_1", "stormline-m", "run/s#1");
    let steps = confirm(&a, &cheap, None);
    assert!(asked(&steps).is_none(), "{steps:?}");
    assert!(
        steps.iter().any(|s| matches!(
            s,
            Step::Standing {
                revision: 1,
                uses_left: None,
                ..
            }
        )),
        "{steps:?}"
    );
    assert_eq!(done(&steps).unwrap()["approved_by"], "standing_permission");
    let sent = confirms(&f);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["approved_by"], "standing_permission");
    let not_covered = |a: &Agent, item: &str, sku: &str, key: &str| {
        let id = propose(a, item, sku, key);
        let steps = confirm(a, &id, None);
        assert!(asked(&steps).is_some(), "{key}: {steps:?}");
        assert!(!standing_used(&steps), "{key}: {steps:?}");
    };
    not_covered(&a, "itm_2", "stormline-insulated-m", "dear");
    f.control(json!({"operations": {"extra_term": "gift_wrap"}}));
    not_covered(&a, "itm_3", "stormline-m", "extra-key");
    assert_eq!(confirms(&f).len(), 1, "only the covered one was confirmed");
    let g = fake(&[]);
    g.control(json!({"operations": {"no_terms": true}}));
    let b = operating(&g, &cheap_exchanges(""));
    not_covered(&b, "itm_4", "stormline-m", "no-terms");
    let h = fake(&[]);
    h.control(json!({"operations": {"user_approval_required": true}}));
    let c = operating(&h, &cheap_exchanges(""));
    not_covered(&c, "itm_5", "stormline-m", "required");
    assert!(confirms(&g).is_empty() && confirms(&h).is_empty());
    // The Company requires the User's own approval only when the confirm
    // arrives: asked, and never by the permission again.
    let i = fake(&[]);
    i.control(json!({"operations": {"refuse_standing": 1}}));
    let d = operating(&i, &cheap_exchanges(""));
    let id = propose(&d, "itm_6", "stormline-m", "late");
    let steps = confirm(&d, &id, None);
    assert!(
        steps.contains(&Step::Confirmed {
            revision: 1,
            approved_by: "standing_permission".into(),
            outcome: Went::Refused,
            state: None,
            error: Some("user_approval_required".into())
        }),
        "{steps:?}"
    );
    let (q, _) = asked(&steps).unwrap();
    assert!(q.contains("only with your own approval"), "{q}");
    let steps = confirm(&d, &id, None);
    assert!(asked(&steps).is_some() && !standing_used(&steps));
    assert_eq!(
        confirms(&i).len(),
        1,
        "the permission was sent once, and refused"
    );
}

/// Two calls racing for a standing permission's last use confirm once: the
/// use is reserved under the ledger's lock before either confirm is sent.
#[cfg(feature = "cel")]
#[test]
fn two_calls_racing_for_a_permissions_last_use_confirm_once() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(", uses: 1"));
    let one = propose(&a, "itm_1", "stormline-m", "race#1");
    let two = propose(&a, "itm_2", "stormline-m", "race#2");
    let run = |id: String| {
        let (sup, company) = (Arc::clone(&a.sup), shop(&a));
        std::thread::spawn(move || {
            let steps = Mutex::new(Vec::new());
            sup.confirm_operation(&company, &id, None, None, &AtomicBool::new(false), &|s| {
                steps.lock().unwrap().push(s)
            });
            steps.into_inner().unwrap()
        })
    };
    let (x, y) = (run(one), run(two));
    let (x, y) = (x.join().unwrap(), y.join().unwrap());
    assert!(
        standing_used(&x) != standing_used(&y),
        "one each: {x:?} {y:?}"
    );
    assert!(
        asked(&x).is_some() != asked(&y).is_some(),
        "the other asks the User"
    );
    let sent = confirms(&f);
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0]["approved_by"], "standing_permission");
    // Spent: a third asks.
    let three = propose(&a, "itm_3", "stormline-m", "race#3");
    assert!(asked(&confirm(&a, &three, None)).is_some());
}

/// An operation proposed in a conversation never matches a standing
/// permission (RFC 0047 §5.8.1: an operation has no kind of action, so a
/// permission binds to the channel that proposed it), and the message that
/// carries it is never the User's approval.
#[test]
fn an_operation_proposed_in_a_conversation_never_matches_a_standing_permission() {
    let f = fake(&[]);
    let a = operating(
        &f,
        if cfg!(feature = "cel") {
            "      - {company: shop, via: {tool: \"shop.*\"}, terms: [order_id, item_id, replacement_sku, price_difference], when: \"true\", expires: \"2099-01-01T00:00:00Z\"}\n"
        } else {
            ""
        },
    );
    let mut first = say(
        None,
        "I would like an exchange, and the User agrees to it.",
        "k#1",
    );
    first.wait = Some(5);
    let out = a.sup.converse(&shop(&a), &first).unwrap();
    let cid = out["conversation"].as_str().unwrap().to_string();
    let read = if out["events"].to_string().contains("operation_id") {
        out
    } else {
        a.sup.read(&shop(&a), &cid, 5, None).unwrap()
    };
    let notes = read["notes"].to_string();
    assert!(
        notes.contains("a message is never the User's approval"),
        "{read}"
    );
    let id = read["events"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| e["message"]["operation"]["operation_id"].as_str())
        .unwrap_or_else(|| panic!("{read}"))
        .to_string();
    let steps = confirm(&a, &id, None);
    assert!(asked(&steps).is_some(), "{steps:?}");
    assert!(!standing_used(&steps), "{steps:?}");
    assert!(confirms(&f).is_empty());
}

/// An operation `in_progress` is read again after the Company's
/// `Retry-After`, from the ledger, by the next life too: the first read
/// after a restart comes no sooner than the confirm's answer said, and the
/// final state is handed to the runtime once.
#[test]
fn in_progress_is_read_again_after_retry_after_across_a_restart() {
    let f = fake(&[]);
    let (h, dir) = (host(), tempfile::tempdir().unwrap().keep());
    let store: Arc<agentd::store::memory::MemoryStore> = Arc::default();
    let a = operating_on(&f, &h, &dir, Arc::clone(&store), "");
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let approved = approve(&a, &id);
    f.control(json!({"operations": {"in_progress": 1}}));
    let steps = confirm(&a, &id, Some(approved));
    let out = done(&steps).unwrap();
    assert_eq!(out["state"], "in_progress", "{out}");
    assert_eq!(out["reading"], true);
    let answered = confirms(&f)[0]["at_ms"].as_u64().unwrap();
    drop(a);
    let b = operating_on(&f, &h, &dir, store, "");
    let finals: Arc<Mutex<Vec<String>>> = Arc::default();
    let into = Arc::clone(&finals);
    b.sup.notify_to(Arc::new(move |n| {
        if let agentd::companies::Notice::Final(f) = n {
            into.lock()
                .unwrap()
                .push(format!("{}:{}", f.operation_id, f.state));
            let _ = f.ack.try_send(Ok(()));
        }
    }));
    b.sup.track_operations();
    until("the final state handed on", || {
        !finals.lock().unwrap().is_empty()
    });
    // The reads after the confirm's answer (one came before it: the confirm
    // reads the operation first).
    let reads: Vec<Value> = f
        .record()
        .into_iter()
        .filter(|r| {
            r["operation_request"] == true
                && r["method"] == "GET"
                && r["at_ms"].as_u64() > Some(answered)
        })
        .collect();
    assert!(!reads.is_empty());
    let first = reads[0]["at_ms"].as_u64().unwrap();
    assert!(
        first >= answered + 2_000,
        "read {} ms after the answer, which said 2 s",
        first - answered
    );
    assert_eq!(*finals.lock().unwrap(), [format!("{id}:succeeded")]);
    let read = b.sup.get_operation(&shop(&b), &id, None).unwrap();
    assert_eq!(read["state"], "succeeded");
    assert_eq!(read["reading"], false);
    b.sup.stop_readers();
}

/// After a confirm that got no answer, the same operation is confirmed again
/// with the same id and revision, and read; the request that proposed it is
/// refused unsent while it is not final, and an operation of the same terms
/// through another channel is not confirmed — until it is final.
#[test]
fn after_a_confirm_timeout_nothing_proposes_the_same_action_until_it_is_final() {
    let f = fake(&[]);
    let a = operating(&f, "");
    let (client, _) =
        agentd::companies::mcp::connect(&a.sup, "shop", Duration::from_secs(20)).unwrap();
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let approved = approve(&a, &id);
    f.control(json!({"operations": {"in_progress": 30}}));
    f.control(json!({"http_fail": {"method": "POST", "path": "/poppy/operations/", "times": 3, "status": 502}}));
    let steps = confirm(&a, &id, Some(approved.clone()));
    let e = done(&steps).unwrap_err();
    assert!(e.starts_with("outcome unknown: "), "{e}");
    assert!(
        e.contains("company.operation.confirm sends this confirm again"),
        "{e}"
    );
    let sent = confirms(&f);
    assert_eq!(sent.len(), 3, "the transport's attempts: {sent:#?}");
    for s in &sent {
        assert_eq!(s["operation_id"], id.as_str());
        assert_eq!(s["revision"], 1);
    }
    // The request that proposed it is not sent again.
    let before = f.record().len();
    let e = a
        .sup
        .call_api(
            &shop(&a),
            &Call {
                method: "POST".into(),
                path: "/orders/ord_7Hk2/exchanges".into(),
                json: Some(
                    json!({"item_id": "itm_4Qa", "replacement_sku": "stormline-insulated-m"}),
                ),
                key: "run/p#2".into(),
                ..Call::default()
            },
        )
        .unwrap_err();
    assert!(matches!(e, CallError::Unsent(_)), "{e:?}");
    assert!(
        e.to_string().contains(&format!(
            "operation {id}, which this request proposed, is still"
        )),
        "{e}"
    );
    assert_eq!(f.record().len(), before, "nothing reached the Company");
    // The same terms through another channel: kept, and never confirmed
    // while the first is open.
    let r = client
        .call_tool(
            "exchange_item",
            Some(json!({"order_id": "ord_7Hk2", "item_id": "itm_4Qa", "replacement_sku": "stormline-insulated-m"})),
        )
        .unwrap();
    let other = a
        .sup
        .recognise(
            &shop(&a),
            Channel::Tool {
                name: "shop.exchange_item".into(),
            },
            None,
            &r.structured_content.unwrap()["operation"],
        )
        .unwrap();
    assert_ne!(other, id);
    let e = done(&confirm(&a, &other, Some(approved))).unwrap_err();
    assert!(
        e.contains(&format!("operation {id}, with the same terms, is still")),
        "{e}"
    );
    // Confirmed again: read with the same id, found confirmed and in
    // progress, and no confirm sent.
    let again = done(&confirm(&a, &id, None)).unwrap();
    assert_eq!(again["state"], "in_progress", "{again}");
    assert_eq!(confirms(&f).len(), 3);
    // Once it is final, both let go.
    until("the operation final", || {
        a.sup
            .get_operation(&shop(&a), &id, None)
            .is_ok_and(|v| v["state"] == "succeeded")
    });
    let steps = confirm(&a, &other, None);
    assert!(asked(&steps).is_some(), "{steps:?}");
}

/// A Direct Conversation (§7.10) carries the User's words, and only theirs,
/// as `sender: human`: the first message, linked to the conversation it
/// follows, and each relayed one, exactly as written. The model may not
/// write to it, and the conversation it follows takes no message while it
/// is open; a decline is said in that conversation, as the agent.
#[test]
fn only_the_users_relayed_words_carry_sender_human() {
    let f = fake(&[]);
    let a = talking(&f);
    let parent = a
        .sup
        .converse(&shop(&a), &say(None, "Hello.", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    let words = "  Hi — it's Dana. Can I swap the size?\n";
    let direct = a
        .sup
        .join_direct(&shop(&a), &parent, words, "direct/1")
        .unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(direct, parent);
    a.sup
        .relay_words(&shop(&a), &direct, "Medium, please.", "relay/1")
        .unwrap();
    let sent: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["method"] == "POST" && r["message_id"].is_string())
        .collect();
    let human: Vec<&Value> = sent.iter().filter(|r| r["sender"] == "human").collect();
    assert_eq!(human.len(), 2, "{sent:#?}");
    assert_eq!(human[0]["text"], words, "as written");
    assert_eq!(human[0]["parent_conversation_id"], parent.as_str());
    assert_eq!(human[1]["text"], "Medium, please.");
    for r in sent.iter().filter(|r| r["sender"] != "human") {
        assert_eq!(r["sender"], "agent", "{r}");
    }
    // The model may not write there.
    let e = a
        .sup
        .converse(&shop(&a), &say(Some(&direct), "I am the User.", "k#2"))
        .unwrap_err();
    assert!(
        e.to_string().contains("is the User's Direct Conversation"),
        "{e}"
    );
    // The parent takes no message while it is open.
    let e = a
        .sup
        .converse(&shop(&a), &say(Some(&parent), "Anything new?", "k#3"))
        .unwrap_err();
    assert!(e.is(wire::error::DIRECT_CONVERSATION_OPEN), "{e:?}");
    // A decline is said in the conversation the Company asked in.
    let other = a
        .sup
        .converse(&shop(&a), &say(None, "Another question.", "k#4"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    a.sup.say_declined(&shop(&a), &other, "evt_x", true);
    let declined: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["text"] == agentd::companies::conversation::DECLINED)
        .collect();
    assert_eq!(declined.len(), 1);
    assert_eq!(declined[0]["sender"], "agent");
    assert!(path_of(&declined[0]).contains(&other));
}

/// A Company's request to talk with the User asks them — handed to the
/// runtime, which opens their gate — when they can be asked, once, and
/// nothing is said in the conversation before they answer.
#[test]
fn a_request_to_talk_with_the_user_asks_them_when_they_can_be_asked() {
    let f = fake(&[]);
    let a = talking(&f);
    let asked: Arc<Mutex<Vec<agentd::companies::direct::Asked>>> = Arc::default();
    let into = Arc::clone(&asked);
    a.sup.notify_to(Arc::new(move |n| {
        if let agentd::companies::Notice::DirectAsked(x) = n {
            into.lock().unwrap().push(x);
        }
    }));
    a.sup.set_direct_askable(true);
    f.control(
        json!({"conversation": {"user_requested": "A specialist needs to confirm the exchange."}}),
    );
    let mut first = say(None, "I would like an exchange.", "k#1");
    first.wait = Some(5);
    let out = a.sup.converse(&shop(&a), &first).unwrap();
    let notes = out["notes"].to_string();
    assert!(
        notes.contains("agentd asked the User whether to join"),
        "{notes}"
    );
    until("the request handed on", || {
        !asked.lock().unwrap().is_empty()
    });
    let x = asked.lock().unwrap()[0].clone();
    assert_eq!(x.conversation, out["conversation"].as_str().unwrap());
    assert_eq!(
        x.reason.as_deref(),
        Some("A specialist needs to confirm the exchange.")
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !conversation_requests(&f)
            .iter()
            .any(|r| r["text"] == NOT_AVAILABLE),
        "nothing said for the User before they answer"
    );
    assert_eq!(asked.lock().unwrap().len(), 1, "once");
}

/// A store that refuses the next write of a Company record once it is
/// armed, as a full one would.
#[derive(Default)]
struct Refusing {
    inner: agentd::store::memory::MemoryStore,
    armed: AtomicBool,
}

impl agentd::store::Store for Refusing {
    fn put(
        &self,
        key: &str,
        seq: u64,
        envelope: &Value,
    ) -> Result<agentd::store::PutOutcome, agentd::store::StoreError> {
        if key.contains("/company/") && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(agentd::store::StoreError::Mapping(
                "the store is full".into(),
            ));
        }
        self.inner.put(key, seq, envelope)
    }
    fn get(&self, key: &str, seq: Option<u64>) -> Result<Option<Value>, agentd::store::StoreError> {
        self.inner.get(key, seq)
    }
    fn list(&self, prefix: &str) -> Result<Vec<agentd::store::KeySeq>, agentd::store::StoreError> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), agentd::store::StoreError> {
        self.inner.delete(key)
    }
    fn kind(&self) -> &'static str {
        "memory"
    }
}

/// How a confirm's steps said its confirm went.
fn went(steps: &[Step]) -> Vec<Went> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Confirmed { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .collect()
}

/// A standing permission is asked again at each confirm (Ops §5.1: it MUST
/// "still be in effect"): a confirm by it that the Company refused before
/// doing anything keeps no approval and gives its use back, so another
/// operation takes the use and the next confirm of the first asks the User.
#[cfg(feature = "cel")]
#[test]
fn a_refused_confirm_gives_its_use_back_and_approves_nothing_more() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(", uses: 1"));
    let x = propose(&a, "itm_1", "stormline-m", "x#1");
    f.control(json!({"operations": {"insufficient_scope": 1}}));
    let steps = confirm(&a, &x, None);
    assert!(standing_used(&steps), "{steps:?}");
    assert_eq!(went(&steps), [Went::Refused], "{steps:?}");
    assert!(done(&steps).is_err());
    let view = a.sup.get_operation(&shop(&a), &x, None).unwrap();
    assert!(view["approved_by"].is_null(), "no approval is kept: {view}");
    let y = propose(&a, "itm_2", "stormline-m", "y#1");
    let steps = confirm(&a, &y, None);
    assert!(standing_used(&steps), "the use came back: {steps:?}");
    assert_eq!(done(&steps).unwrap()["state"], "succeeded");
    let steps = confirm(&a, &x, None);
    assert!(
        asked(&steps).is_some() && !standing_used(&steps),
        "its one use is spent: {steps:?}"
    );
    let sent = confirms(&f);
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[1]["operation_id"], y.as_str());
}

/// A confirm by a standing permission that got no answer is not sent again
/// on its old approval: once the permission no longer applies — suspended,
/// the User having signed in to a new account there — the next confirm asks
/// the User and sends nothing.
#[cfg(feature = "cel")]
#[test]
fn a_standing_approval_is_asked_again_at_each_confirm() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(""));
    let x = propose(&a, "itm_1", "stormline-m", "x#1");
    f.control(json!({"operations": {"drop": 3}}));
    let steps = confirm(&a, &x, None);
    assert!(standing_used(&steps), "{steps:?}");
    assert_eq!(went(&steps), [Went::Unknown], "{steps:?}");
    let e = done(&steps).unwrap_err();
    assert!(e.starts_with("outcome unknown: "), "{e}");
    assert_eq!(confirms(&f).len(), 3, "the transport's attempts");
    a.sup.suspend_standing("shop");
    let steps = confirm(&a, &x, None);
    assert!(
        asked(&steps).is_some() && !standing_used(&steps),
        "{steps:?}"
    );
    assert_eq!(confirms(&f).len(), 3, "nothing sent on the old approval");
}

/// A standing permission's use follows what the Company says of the
/// operation it is reserved for: given back once the operation is cancelled
/// unconfirmed, spent once a confirm of it was taken — whatever became of
/// that confirm's answer — and a cancel after that gives nothing back.
#[cfg(feature = "cel")]
#[test]
fn a_permissions_use_follows_what_the_company_says_of_its_operation() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(", uses: 2"));
    // Unanswered, and never reached the action.
    let x = propose(&a, "itm_1", "stormline-m", "x#1");
    f.control(json!({"operations": {"drop": 3}}));
    assert!(
        done(&confirm(&a, &x, None))
            .unwrap_err()
            .starts_with("outcome unknown: ")
    );
    // Unanswered, and taken.
    let z = propose(&a, "itm_2", "stormline-m", "z#1");
    f.control(json!({"operations": {"in_progress": 30}}));
    f.control(json!({"http_fail": {"method": "POST", "path": "/poppy/operations/", "times": 3, "status": 502}}));
    assert!(
        done(&confirm(&a, &z, None))
            .unwrap_err()
            .starts_with("outcome unknown: ")
    );
    // Both uses are held for them.
    let y = propose(&a, "itm_3", "stormline-m", "y#1");
    assert!(asked(&confirm(&a, &y, None)).is_some());
    let out = a.sup.cancel_operation(&shop(&a), &x, None).unwrap();
    assert_eq!(out["state"], "cancelled", "{out}");
    let out = a.sup.cancel_operation(&shop(&a), &z, None).unwrap();
    assert_eq!(
        out["state"], "in_progress",
        "confirmed, so not stopped: {out}"
    );
    // X's use came back; Z's is spent.
    let w = propose(&a, "itm_4", "stormline-m", "w#1");
    let steps = confirm(&a, &w, None);
    assert!(standing_used(&steps), "{steps:?}");
    let v = propose(&a, "itm_5", "stormline-m", "v#1");
    let steps = confirm(&a, &v, None);
    assert!(
        asked(&steps).is_some() && !standing_used(&steps),
        "both uses spent: {steps:?}"
    );
}

/// A confirm the Company answers with the operation as it was — cancelled
/// already, on its site — confirmed nothing (Ops §5.2: it "returns the
/// operation as it is and does nothing else"): its outcome says so, and the
/// permission's use goes back for another operation.
#[cfg(feature = "cel")]
#[test]
fn a_confirm_the_company_did_nothing_with_spends_no_use() {
    let f = fake(&[]);
    let a = operating(&f, &cheap_exchanges(", uses: 1"));
    let x = propose(&a, "itm_1", "stormline-m", "x#1");
    f.control(json!({"operations": {"cancel_first": 1}}));
    let steps = confirm(&a, &x, None);
    assert!(
        steps.iter().any(|s| matches!(s,
            Step::Confirmed { outcome: Went::Untaken, state: Some(st), .. } if st == "cancelled")),
        "{steps:?}"
    );
    let out = done(&steps).unwrap();
    assert!(
        out["note"]
            .as_str()
            .unwrap()
            .contains("did nothing with the confirm"),
        "{out}"
    );
    let y = propose(&a, "itm_2", "stormline-m", "y#1");
    let steps = confirm(&a, &y, None);
    assert!(standing_used(&steps), "the use was not spent: {steps:?}");
    assert_eq!(went(&steps), [Went::Taken]);
}

/// The User's approval covers what the gate showed of a revision: when the
/// Company says otherwise under the same revision — which Ops §3.1 forbids —
/// the approval covers nothing, nothing is sent, and the User is asked
/// again, the change shown.
#[test]
fn an_approval_covers_what_the_gate_showed_of_its_revision() {
    let f = fake(&[]);
    let a = operating(&f, "");
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let first = approve(&a, &id);
    f.control(json!({"operations": {"rewrite": 1}}));
    let steps = confirm(&a, &id, Some(first.clone()));
    let (q, revision) = asked(&steps).unwrap_or_else(|| panic!("{steps:?}"));
    assert_eq!(revision, 1);
    assert!(
        q.contains("The Company changed what this revision says after you were asked"),
        "{q}"
    );
    assert!(
        q.contains("\"price_difference.amount\": \"70.00\" → \"150.00\""),
        "{q}"
    );
    assert!(confirms(&f).is_empty(), "nothing sent");
    assert!(asked(&confirm(&a, &id, Some(first))).is_some());
    let ok = confirm(&a, &id, Some(approval(&steps)));
    assert_eq!(done(&ok).unwrap()["state"], "succeeded");
    let sent = confirms(&f);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["revision"], 1);
    // An approval kept for a confirm that got no answer is shown as one
    // only while the Company says what it approved.
    let other = propose(&a, "itm_9", "stormline-insulated-m", "run/p#2");
    let approved = approve(&a, &other);
    f.control(json!({"operations": {"drop": 3}}));
    assert!(done(&confirm(&a, &other, Some(approved))).is_err());
    let view = a.sup.get_operation(&shop(&a), &other, None).unwrap();
    assert_eq!(view["approved_by"], "user", "{view}");
    f.control(json!({"operations": {"rewrite": 1}}));
    let view = a.sup.get_operation(&shop(&a), &other, None).unwrap();
    assert!(view["approved_by"].is_null(), "{view}");
}

/// A confirm the Company answered whose answer agentd could not keep — the
/// store refusing the write — is `outcome unknown`, never unsent: its lock
/// is let go and it is read again, so its final state is handed on.
#[test]
fn a_confirm_whose_answer_is_not_kept_is_outcome_unknown() {
    let f = fake(&[]);
    let store: Arc<Refusing> = Arc::default();
    let dir = tempfile::tempdir().unwrap().keep();
    let a = operating_with(&f, &host(), &dir, Arc::clone(&store) as _, "");
    let id = propose(&a, "itm_4Qa", "stormline-insulated-m", "run/p#1");
    let approved = approve(&a, &id);
    f.control(json!({"http_fail": {"method": "POST", "path": "/poppy/operations/", "times": 1, "ms": 1500}}));
    let (sup, company) = (Arc::clone(&a.sup), shop(&a));
    let (cid, given) = (id.clone(), approved);
    let run = std::thread::spawn(move || {
        let steps = Mutex::new(Vec::new());
        sup.confirm_operation(
            &company,
            &cid,
            Some(given),
            None,
            &AtomicBool::new(false),
            &|s| steps.lock().unwrap().push(s),
        );
        steps.into_inner().unwrap()
    });
    // The fake's own line for the hold, which `Fake::record` leaves out.
    until("the confirm taken", || {
        get(&format!("{}/_mock/record", f.origin)).1["records"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["path"] == "/_mock/held")
    });
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let steps = run.join().unwrap();
    assert_eq!(went(&steps), [Went::Unknown], "{steps:?}");
    let e = done(&steps).unwrap_err();
    assert!(
        e.starts_with("outcome unknown: ") && e.contains("could not keep the answer"),
        "{e}"
    );
    let finals: Arc<Mutex<Vec<String>>> = Arc::default();
    let into = Arc::clone(&finals);
    a.sup.notify_to(Arc::new(move |n| {
        if let agentd::companies::Notice::Final(f) = n {
            into.lock().unwrap().push(f.state.clone());
            let _ = f.ack.try_send(Ok(()));
        }
    }));
    a.sup.track_operations();
    until("the final state handed on", || {
        !finals.lock().unwrap().is_empty()
    });
    assert_eq!(*finals.lock().unwrap(), ["succeeded"]);
    let view = a.sup.get_operation(&shop(&a), &id, None).unwrap();
    assert_eq!(view["state"], "succeeded");
    assert_eq!(confirms(&f).len(), 1);
    a.sup.stop_readers();
}

/// A Company that asks, in the User's own Direct Conversation, to talk with
/// them directly is asked nothing and told nothing: no question for the
/// User, whether or not they can be asked, and no message of agentd's in a
/// conversation that carries only the User's words.
#[test]
fn a_request_to_talk_in_a_direct_conversation_asks_nothing_and_says_nothing() {
    let f = fake(&[]);
    let a = talking(&f);
    let asked: Arc<Mutex<Vec<agentd::companies::direct::Asked>>> = Arc::default();
    let into = Arc::clone(&asked);
    a.sup.notify_to(Arc::new(move |n| {
        if let agentd::companies::Notice::DirectAsked(x) = n {
            into.lock().unwrap().push(x);
        }
    }));
    let parent = a
        .sup
        .converse(&shop(&a), &say(None, "Hello.", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    let direct = a
        .sup
        .join_direct(&shop(&a), &parent, "Hi, it's Dana.", "direct/1")
        .unwrap();
    for askable in [false, true] {
        a.sup.set_direct_askable(askable);
        f.control(json!({"conversation": {"user_requested": "Shall we talk directly?"}}));
        a.sup
            .relay_words(
                &shop(&a),
                &direct,
                &format!("Still here ({askable})."),
                &format!("relay/{askable}"),
            )
            .unwrap();
        let read = a.sup.read(&shop(&a), &direct, 5, None).unwrap();
        assert!(
            read["notes"]
                .to_string()
                .contains("agentd asked nobody and sent nothing"),
            "{read}"
        );
    }
    // Nor is a decline ever said there.
    a.sup.say_declined(&shop(&a), &direct, "evt_x", true);
    std::thread::sleep(Duration::from_millis(1000));
    assert!(asked.lock().unwrap().is_empty(), "nobody was asked");
    let into_direct: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["method"] == "POST" && path_of(r).contains(&direct))
        .collect();
    assert_eq!(into_direct.len(), 2, "{into_direct:#?}");
    for r in &into_direct {
        assert_eq!(r["sender"], "human", "{r}");
    }
}

/// The User's first message sent again — a restart while it was on its way
/// — under the id it was sent with starts no other conversation: the
/// Company answers with the Direct Conversation it started (§7.3).
#[test]
fn a_first_message_sent_again_starts_no_other_direct_conversation() {
    let f = fake(&[]);
    let a = talking(&f);
    let parent = a
        .sup
        .converse(&shop(&a), &say(None, "Hello.", "k#1"))
        .unwrap()["conversation"]
        .as_str()
        .unwrap()
        .to_string();
    let words = "Hi, it's Dana.";
    let one = a
        .sup
        .join_direct(&shop(&a), &parent, words, "direct/task-1")
        .unwrap();
    let again = a
        .sup
        .join_direct(&shop(&a), &parent, words, "direct/task-1")
        .unwrap();
    assert_eq!(one, again);
    let firsts: Vec<Value> = conversation_requests(&f)
        .into_iter()
        .filter(|r| r["parent_conversation_id"].is_string())
        .collect();
    assert_eq!(firsts.len(), 2);
    assert_eq!(firsts[0]["message_id"], firsts[1]["message_id"]);
    assert_eq!(firsts[1]["duplicate"], true, "{:#?}", firsts[1]);
}
