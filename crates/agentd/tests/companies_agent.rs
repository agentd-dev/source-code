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
    assert_eq!(names, ["search", "order_status"]);
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
