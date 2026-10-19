// SPDX-License-Identifier: AGPL-3.0-only
//! agentd's Client ID Metadata Document (RFC 0052), end to end, against a
//! mock authorization server on loopback: `--print-client-metadata` writes
//! the document and its key set; an `oauth2` block with no `client_id`
//! presents the document's URL as its `client_id`, authenticated by a
//! `private_key_jwt` assertion that verifies with the printed key set — at
//! the device endpoint, the token endpoint of a `--login`, and the
//! client-credentials grant the daemon's signer makes — but only where the
//! server's metadata advertises `client_id_metadata_document_supported`.
//! Where it does not, the login and the signer refuse before anything is
//! sent, saying what to register. With the `a2a` feature, the listener
//! serves the same two documents at their URLs' paths.
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

const CLIENT_ID: &str = "https://agent.example/oauth/client.json";
const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// What the mock authorization server was asked: each request's method,
/// path and form body.
type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

/// A mock authorization server: its RFC 8414 metadata — advertising the
/// document or not — a device endpoint, and a token endpoint that answers
/// every grant at once.
fn spawn_as(advertise: bool) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (me, log) = (base.clone(), Arc::clone(&seen));
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            serve(c, &me, advertise, &log);
        }
    });
    (base, seen)
}

fn serve(stream: TcpStream, base: &str, advertise: bool, seen: &Seen) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    let form = String::from_utf8_lossy(&body).into_owned();
    seen.lock()
        .unwrap()
        .push((method, path.clone(), form.clone()));
    let (status, payload) = match path.as_str() {
        "/.well-known/oauth-authorization-server" => {
            let mut m = json!({
                "issuer": base,
                "token_endpoint": format!("{base}/token"),
                "device_authorization_endpoint": format!("{base}/device"),
            });
            if advertise {
                m["client_id_metadata_document_supported"] = json!(true);
            }
            ("200 OK", m.to_string())
        }
        "/device" => (
            "200 OK",
            json!({"device_code": "dev-1", "user_code": "CIMD-CODE",
                   "verification_uri": "https://as.example/device", "interval": 1, "expires_in": 60})
            .to_string(),
        ),
        "/token" => match field(&form, "grant_type").as_deref() {
            Some("client_credentials") => (
                "200 OK",
                json!({"access_token": "at-cc", "token_type": "Bearer", "expires_in": 3600})
                    .to_string(),
            ),
            _ => (
                "200 OK",
                json!({"access_token": "at-dev", "refresh_token": "rt-dev",
                       "token_type": "Bearer", "expires_in": 3600})
                .to_string(),
            ),
        },
        _ => ("404 Not Found", "{}".to_string()),
    };
    let mut w = r.into_inner();
    let _ = write!(
        w,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
}

/// A form field, percent-decoded.
fn field(form: &str, key: &str) -> Option<String> {
    let raw = form
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))?;
    let mut out = Vec::new();
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    Some(String::from_utf8(out).unwrap())
}

/// base64url, unpadded.
fn b64url(s: &str) -> Vec<u8> {
    let (mut bits, mut n, mut out) = (0u32, 0, Vec::new());
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => continue,
        };
        bits = bits << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    out
}

/// The claims of a client assertion that verifies, ES256, with the key the
/// key set lists under its header's `kid`; a panic otherwise.
fn verified_claims(assertion: &str, jwks: &Value) -> Value {
    let (input, sig) = assertion.rsplit_once('.').unwrap();
    let header: Value = serde_json::from_slice(&b64url(input.split('.').next().unwrap())).unwrap();
    assert_eq!(header["alg"], "ES256", "{header}");
    let key = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kid"] == header["kid"])
        .unwrap_or_else(|| panic!("the key set has no key {}", header["kid"]));
    let mut point = vec![0x04];
    point.extend(b64url(key["x"].as_str().unwrap()));
    point.extend(b64url(key["y"].as_str().unwrap()));
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(input.as_bytes(), &b64url(sig))
        .expect("the assertion verifies with the published key");
    serde_json::from_slice(&b64url(input.split('.').nth(1).unwrap())).unwrap()
}

/// A configuration whose one MCP server's `oauth2` block names no
/// `client_id`, so agentd's document is its client.
fn config(dir: &std::path::Path, issuer: &str, grant: &str, grants: &str) -> String {
    format!(
        "\
         agent:\n  name: cimd\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
         store:\n  kind: memory\n\
         security:\n  client:\n    id: {CLIENT_ID}\n    name: CIMD test agent\n\
         \x20   key_file: {key}\n    grants: [{grants}]\n\
         mcp:\n  servers:\n    - name: test\n      endpoint: https://mcp.example\n\
         \x20     auth:\n        kind: oauth2\n        grant: {grant}\n        issuer: \"{issuer}\"\n\
         \x20       scopes: [mcp:read]\n\
         lifecycle:\n  run_until: idle\n",
        key = dir.join("client-key.pem").display(),
    )
}

/// `--print-client-metadata` into `<dir>/published`: the document and the
/// key set it wrote.
fn print(dir: &std::path::Path, cfg: &str) -> (Value, Value) {
    let out_dir = dir.join("published");
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg, "--print-client-metadata"])
        .arg(&out_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--print-client-metadata: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        printed.contains("client.json") && printed.contains("jwks.json"),
        "{printed}"
    );
    let read = |f: &str| -> Value {
        serde_json::from_slice(&std::fs::read(out_dir.join(f)).unwrap()).unwrap()
    };
    (read("client.json"), read("jwks.json"))
}

/// The settings the binary loads, for the daemon's own signer path.
fn settings(cfg: &str) -> agentd::config::settings::Settings {
    let (loaded, _) =
        agentd::config::settings::load(&["--config".to_string(), cfg.to_string()], &[]).unwrap();
    loaded.settings
}

/// The document carries what the draft and MCP's client registration ask
/// for, and no secret; its key set holds public halves only; and a device
/// login presents the document's URL as `client_id`, signing at the device
/// endpoint and at the token endpoint with the key the set publishes. The
/// login is kept with the issuer it came from.
#[test]
fn a_device_login_presents_the_document_and_signs_with_its_key() {
    let (issuer, seen) = spawn_as(true);
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(&cfg, config(dir.path(), &issuer, "device", "device")).unwrap();
    let cfg = cfg.display().to_string();
    let (doc, jwks) = print(dir.path(), &cfg);
    assert_eq!(
        doc,
        json!({
            "client_id": CLIENT_ID,
            "client_name": "CIMD test agent",
            "jwks_uri": "https://agent.example/oauth/jwks.json",
            "redirect_uris": [],
            "grant_types": [DEVICE_CODE, "refresh_token"],
            "response_types": [],
            "token_endpoint_auth_method": "private_key_jwt",
        })
    );
    let keys = jwks["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].get("d").is_none(), "public halves only: {jwks}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("client-key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the key is the owner's alone");
    }
    assert!(seen.lock().unwrap().is_empty(), "printing dials nothing");

    let creds = dir.path().join("creds");
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &creds)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "login: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = seen.lock().unwrap().clone();
    let device = seen
        .iter()
        .find(|(_, p, _)| p == "/device")
        .expect("a device request");
    let token = seen
        .iter()
        .find(|(_, p, _)| p == "/token")
        .expect("a token request");
    for (_, path, form) in [device, token] {
        assert_eq!(
            field(form, "client_id").as_deref(),
            Some(CLIENT_ID),
            "{path}: {form}"
        );
        assert!(!form.contains("client_secret"), "{path}: {form}");
        assert_eq!(
            field(form, "client_assertion_type").as_deref(),
            Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
            "{path}: {form}"
        );
        let claims = verified_claims(&field(form, "client_assertion").unwrap(), &jwks);
        assert_eq!(claims["iss"], CLIENT_ID);
        assert_eq!(claims["sub"], CLIENT_ID);
        assert_eq!(
            claims["aud"],
            format!("{issuer}/token"),
            "the token endpoint, by default"
        );
    }
    assert_eq!(field(&token.2, "grant_type").as_deref(), Some(DEVICE_CODE));
    let id = agentd::auth::cache::cred_id("mcp:test");
    let record: Value =
        serde_json::from_slice(&std::fs::read(creds.join(format!("{id}.json"))).unwrap()).unwrap();
    assert_eq!(record["access_token"], "at-dev");
    assert_eq!(
        record["authorization_server"],
        issuer.as_str(),
        "kept with its issuer"
    );
}

/// The daemon's signer for a client-credentials block with no `client_id`
/// asks for the client's own token as the document, with an assertion the
/// published key set verifies, and rides it on the request.
#[test]
fn a_client_credentials_grant_presents_the_document() {
    let (issuer, seen) = spawn_as(true);
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        config(
            dir.path(),
            &issuer,
            "client_credentials",
            "client_credentials",
        ),
    )
    .unwrap();
    let cfg = cfg.display().to_string();
    let (doc, jwks) = print(dir.path(), &cfg);
    assert_eq!(doc["grant_types"], json!(["client_credentials"]));
    let s = settings(&cfg);
    let spec = s.auth_spec(s.mcp.servers[0].auth.as_ref().unwrap());
    let signer = agentd::auth::device::signer_for(&spec, "mcp:test", Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(
        signer.sign("POST", "mcp.example", "/mcp", b"{}").unwrap(),
        vec![("Authorization".to_string(), "Bearer at-cc".to_string())]
    );
    // Held: a second request asks for no second token.
    signer.sign("POST", "mcp.example", "/mcp", b"{}").unwrap();
    let seen = seen.lock().unwrap().clone();
    let tokens: Vec<&String> = seen
        .iter()
        .filter(|(_, p, _)| p == "/token")
        .map(|(_, _, f)| f)
        .collect();
    assert_eq!(tokens.len(), 1, "{seen:?}");
    let form = tokens[0];
    assert_eq!(
        field(form, "grant_type").as_deref(),
        Some("client_credentials")
    );
    assert_eq!(field(form, "client_id").as_deref(), Some(CLIENT_ID));
    assert_eq!(field(form, "scope").as_deref(), Some("mcp:read"));
    let claims = verified_claims(&field(form, "client_assertion").unwrap(), &jwks);
    assert_eq!(claims["iss"], CLIENT_ID);
}

/// An authorization server whose metadata does not advertise the document
/// is never given it: the login and the daemon's signer read its metadata,
/// send nothing else, and say what the operator registers instead.
#[test]
fn a_server_that_does_not_advertise_the_document_is_refused() {
    let (issuer, seen) = spawn_as(false);
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(
        &cfg,
        config(dir.path(), &issuer, "device", "device, client_credentials"),
    )
    .unwrap();
    let cfg = cfg.display().to_string();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", dir.path().join("creds"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not advertise client_id_metadata_document_supported")
            && stderr.contains("register agentd there")
            && stderr.contains("set the block's `client_id`"),
        "{stderr}"
    );
    std::fs::write(
        &cfg,
        config(
            dir.path(),
            &issuer,
            "client_credentials",
            "device, client_credentials",
        ),
    )
    .unwrap();
    let s = settings(&cfg);
    let spec = s.auth_spec(s.mcp.servers[0].auth.as_ref().unwrap());
    let signer = agentd::auth::device::signer_for(&spec, "mcp:test", Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let e = signer
        .sign("POST", "mcp.example", "/mcp", b"{}")
        .unwrap_err();
    assert!(
        e.starts_with("mcp:test: ")
            && e.contains("does not advertise client_id_metadata_document_supported"),
        "{e}"
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|(_, p, _)| p == "/.well-known/oauth-authorization-server"),
        "only the metadata was read: {seen:?}"
    );
    assert!(!seen.is_empty());
}

/// `security.client.serve`: the A2A listener serves the document and the
/// key set at their URLs' paths, the same bytes the print writes.
#[cfg(feature = "a2a")]
#[test]
fn the_listener_serves_the_document_it_prints() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agentd.yaml");
    let log = dir.path().join("served.log");
    let (mut child, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "agent:\n  name: served\n  instruction: x\n  preflight: never\n\
                 intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
                 store: {{kind: memory}}\n\
                 lifecycle: {{run_until: drained}}\n\
                 observability: {{log_level: info}}\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n  url: https://agent.example\n\
                 security:\n  client:\n    id: {CLIENT_ID}\n    name: Served agent\n\
                 \x20   key_file: {key}\n    serve: true\n",
                key = dir.path().join("client-key.pem").display(),
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.display().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        (child, log.display().to_string())
    });
    let get = |path: &str| {
        agentd::auth::http::get(
            &format!("http://{addr}{path}"),
            &[],
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap()
    };
    let doc = get("/oauth/client.json");
    let set = get("/oauth/jwks.json");
    // SAFETY: a signal to the child this test spawned.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    assert_eq!(doc.status, 200);
    assert_eq!(doc.header("cache-control"), Some("max-age=300"));
    assert_eq!(set.status, 200);
    let (printed, printed_set) = print(dir.path(), &cfg.display().to_string());
    assert_eq!(serde_json::from_slice::<Value>(&doc.body).unwrap(), printed);
    assert_eq!(
        serde_json::from_slice::<Value>(&set.body).unwrap(),
        printed_set
    );
}
