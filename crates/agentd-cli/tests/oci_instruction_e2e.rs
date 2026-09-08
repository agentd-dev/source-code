// SPDX-License-Identifier: AGPL-3.0-only
//! **Instruction from an OCI registry, end to end** (RFC 0040): the real
//! daemon boots against an in-process mock registry, runs the token dance,
//! pulls the manifest and blob, verifies both digests, loads the document —
//! `instruction.loaded` carries the manifest digest that pins the version —
//! and reaches `proc.ready` with the document's workflow armed. With the
//! `decrypt` feature too, the registry serves an age-ENCRYPTED blob and the
//! oci→decrypt→idoc chain is proven in one pass.
#![cfg(all(unix, feature = "oci", feature = "workflow"))]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Minimal Distribution-API mock: token dance, manifest, blob.
fn mock_registry(blob: Vec<u8>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let sha = |b: &[u8]| -> String {
            use std::fmt::Write as _;
            let d = ring::digest::digest(&ring::digest::SHA256, b);
            let mut s = String::new();
            for x in d.as_ref() {
                let _ = write!(s, "{x:02x}");
            }
            s
        };
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let authed = req
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-e2e");
            let manifest = json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "artifactType": "application/vnd.instruction.document.v1",
                "layers": [{
                    "mediaType": "text/markdown; variant=instruction",
                    "digest": format!("sha256:{}", sha(&blob)),
                    "size": blob.len(),
                }],
            })
            .to_string();
            let (status, extra, body): (&str, String, Vec<u8>) = if path.starts_with("/token") {
                ("200 OK", String::new(), br#"{"token":"tok-e2e"}"#.to_vec())
            } else if !authed {
                (
                    "401 Unauthorized",
                    format!(
                        "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\"\r\n"
                    ),
                    b"{}".to_vec(),
                )
            } else if path.contains("/manifests/") {
                ("200 OK", String::new(), manifest.into_bytes())
            } else if path.contains("/blobs/") {
                ("200 OK", String::new(), blob.clone())
            } else {
                ("404 Not Found", String::new(), Vec::new())
            };
            let head = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    port
}

const DOC: &str = "---\nspec: \"1\"\n---\n# Pulled agent\n\nServe pulls.\n\n:::!workflow{name=pulled-drain}\nsteps:\n  start: { kind: manual }\n  done:  { kind: finish, depends_on: [start] }\n:::\n";

/// Boot with a caller-supplied `agent` block (the long instruction form),
/// otherwise identical to [`boot_and_capture`].
fn boot_and_capture_cfg(overlay: Value) -> String {
    let mut cfg = json!({
        "config_version": "1",
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    if let (Some(dst), Some(src)) = (cfg.as_object_mut(), overlay.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    run_with_config(cfg)
}

/// Boot the daemon with the given instruction URI (+ extra config), wait for
/// `proc.ready`, return the captured stderr log.
fn boot_and_capture(uri: &str, extra: Value) -> String {
    let mut cfg = json!({
        "config_version": "1",
        "agent": {"name": "oci-e2e", "preflight": "never", "instruction": uri},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    if let (Some(dst), Some(src)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    run_with_config(cfg)
}

/// Write the config, boot the real binary, and return its captured stderr.
fn run_with_config(cfg: Value) -> String {
    let cfg_path = common::unique_path("oci-e2e", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("oci-e2e", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    // Wait for proc.ready, a logged exit, or the process dying (a config-load
    // refusal exits before the JSON logger exists) — the pull precedes all.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if log.contains("proc.ready")
            || log.contains("proc.exit")
            || child.try_wait().is_ok_and(|s| s.is_some())
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("daemon neither became ready nor exited:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    for p in [cfg_path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    log
}

fn loaded_event(log: &str) -> Option<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "instruction.loaded")
}

#[test]
fn the_daemon_pulls_loads_and_arms_from_a_registry() {
    let port = mock_registry(DOC.as_bytes().to_vec());
    let log = boot_and_capture(&format!("oci://127.0.0.1:{port}/acme/agent:v1"), json!({}));
    let ev = loaded_event(&log).unwrap_or_else(|| panic!("no instruction.loaded in:\n{log}"));
    assert!(
        ev["manifest_digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:")),
        "the load records its version pin: {ev}"
    );
    assert!(log.contains("proc.ready"), "startup completed:\n{log}");
    assert!(
        log.contains("pulled-drain"),
        "the pulled document's workflow registered:\n{log}"
    );
}

#[test]
fn a_registry_that_cannot_serve_the_document_fails_startup_loudly() {
    // Nothing listens on the port: the pull fails at CONFIG LOAD, and the
    // refusal names the reference — never a daemon running with no instruction.
    let port = common::free_port();
    let log = boot_and_capture(&format!("oci://127.0.0.1:{port}/acme/agent:v1"), json!({}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(log.contains("oci://") && log.contains("connect"), "{log}");
}

/// The full chain — registry serves CIPHERTEXT, agentd decrypts on the fly.
#[cfg(feature = "decrypt")]
#[test]
fn an_encrypted_artifact_pulls_and_decrypts_in_one_pass() {
    use agentd::config::decrypt::{agefile, x25519};
    let sk = [31u8; 32];
    let pk = x25519::public_key(&sk);
    let enc = agefile::encrypt(DOC.as_bytes(), &[pk], None).unwrap();
    let port = mock_registry(enc);

    let key_path = common::unique_path("oci-age-key", "txt");
    std::fs::write(&key_path, agefile::encode_identity(&sk)).unwrap();
    // The key that opens the artifact lives with the artifact reference —
    // `agent.instruction` carries the source and everything about it.
    let log = boot_and_capture_cfg(json!({
        "agent": {"name": "oci-e2e", "preflight": "never",
                  "instruction": {"oci": format!("127.0.0.1:{port}/acme/agent:v1"),
                                  "decrypt": {"keys": [key_path.clone()]}}},
    }));
    assert!(
        loaded_event(&log).is_some() && log.contains("pulled-drain"),
        "encrypted artifact decrypted and loaded:\n{log}"
    );
    let _ = std::fs::remove_file(&key_path);
}

// ── cosign: the artifact's own signature ────────────────────────────────────
//
// `trust` answers who WROTE the document; cosign answers who PUSHED the
// artifact. These prove the second, through the real daemon: the registry
// serves what cosign would have pushed beside the artifact — the
// `sha256-….sig` tag, a simple-signing payload blob, and the base64 signature
// in the layer annotation — and agentd either boots or refuses.

fn sha_hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = ring::digest::digest(&ring::digest::SHA256, b);
    let mut s = String::new();
    for x in d.as_ref() {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A P-256 key pair, and the PEM `PUBLIC KEY` file an operator would configure.
fn cosign_keypair() -> (ring::signature::EcdsaKeyPair, String) {
    let rng = ring::rand::SystemRandom::new();
    let alg = &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
    let kp = ring::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap();
    let point = {
        use ring::signature::KeyPair;
        kp.public_key().as_ref().to_vec()
    };
    // SPKI(P-256): the 26-byte prefix cosign writes, then the point.
    let mut der = vec![
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    der.extend_from_slice(&point);
    let path = common::unique_path("cosign", "pub");
    std::fs::write(
        &path,
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            b64(&der)
        ),
    )
    .unwrap();
    (kp, path)
}

/// How the mock should sign what it serves.
enum Sig<'a> {
    /// No `.sig` tag at all — the unsigned artifact.
    None,
    /// A real signature over the manifest digest.
    Over(&'a ring::signature::EcdsaKeyPair),
    /// A real signature by the right key, over SOMEBODY ELSE's digest.
    OverOther(&'a ring::signature::EcdsaKeyPair),
}

/// The mock registry, plus the cosign signature layout beside the artifact.
fn mock_registry_cosign(doc: Vec<u8>, sig: Sig<'_>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let doc_digest = format!("sha256:{}", sha_hex(&doc));
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.instruction.document.v1",
        "layers": [{
            "mediaType": "text/markdown; variant=instruction",
            "digest": doc_digest, "size": doc.len(),
        }],
    })
    .to_string();
    let manifest_digest = format!("sha256:{}", sha_hex(manifest.as_bytes()));

    let mut blobs: std::collections::BTreeMap<String, Vec<u8>> = std::collections::BTreeMap::new();
    blobs.insert(doc_digest, doc);

    let sig_manifest = match sig {
        Sig::None => None,
        Sig::Over(kp) | Sig::OverOther(kp) => {
            let covered = match sig {
                Sig::OverOther(_) => "sha256:".to_string() + &"11".repeat(32),
                _ => manifest_digest.clone(),
            };
            let payload = json!({
                "critical": {
                    "identity": {"docker-reference": "127.0.0.1/acme/agent"},
                    "image": {"docker-manifest-digest": covered},
                    "type": "cosign container image signature"},
                "optional": Value::Null,
            })
            .to_string();
            let rng = ring::rand::SystemRandom::new();
            let signature = kp.sign(&rng, payload.as_bytes()).unwrap();
            let pd = format!("sha256:{}", sha_hex(payload.as_bytes()));
            blobs.insert(pd.clone(), payload.clone().into_bytes());
            Some(
                json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "layers": [{
                        "mediaType": "application/vnd.dev.cosign.simplesigning.v1+json",
                        "digest": pd, "size": payload.len(),
                        "annotations": {
                            "dev.cosignproject.cosign/signature": b64(signature.as_ref()),
                        },
                    }],
                })
                .to_string(),
            )
        }
    };
    let sig_tag = format!("{}.sig", manifest_digest.replace(':', "-"));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let authed = req
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-e2e");
            let (status, extra, body): (&str, String, Vec<u8>) = if path.starts_with("/token") {
                ("200 OK", String::new(), br#"{"token":"tok-e2e"}"#.to_vec())
            } else if !authed {
                (
                    "401 Unauthorized",
                    format!(
                        "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\"\r\n"
                    ),
                    b"{}".to_vec(),
                )
            } else if path.ends_with(&sig_tag) {
                match &sig_manifest {
                    Some(m) => ("200 OK", String::new(), m.clone().into_bytes()),
                    None => ("404 Not Found", String::new(), Vec::new()),
                }
            } else if path.contains("/manifests/") {
                ("200 OK", String::new(), manifest.clone().into_bytes())
            } else if let Some(d) = path.split("/blobs/").nth(1) {
                match blobs.get(d) {
                    Some(b) => ("200 OK", String::new(), b.clone()),
                    None => ("404 Not Found", String::new(), Vec::new()),
                }
            } else {
                ("404 Not Found", String::new(), Vec::new())
            };
            let head = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    port
}

/// The happy path, all the way through the real daemon: a signed artifact with
/// the matching key configured pulls, verifies, loads and arms.
#[test]
fn a_cosign_signed_artifact_verifies_and_boots() {
    let (kp, key_path) = cosign_keypair();
    let port = mock_registry_cosign(DOC.as_bytes().to_vec(), Sig::Over(&kp));
    let log = boot_and_capture_cfg(json!({"agent": {
        "name": "oci-cosign", "preflight": "never",
        "instruction": {"oci": {"ref": format!("127.0.0.1:{port}/acme/agent:v1"),
                                "cosign_key": key_path}}}}));
    assert!(
        log.contains("proc.ready"),
        "a valid signature boots:\n{log}"
    );
    assert!(log.contains("pulled-drain"), "the document loaded:\n{log}");
    let _ = std::fs::remove_file(&key_path);
}

/// With a key configured, an artifact nobody signed is refused — the check
/// cannot be satisfied by the registry simply not offering a signature.
#[test]
fn an_unsigned_artifact_is_refused_when_a_key_is_configured() {
    let (_kp, key_path) = cosign_keypair();
    let port = mock_registry_cosign(DOC.as_bytes().to_vec(), Sig::None);
    let log = boot_and_capture_cfg(json!({"agent": {
        "name": "oci-cosign", "preflight": "never",
        "instruction": {"oci": {"ref": format!("127.0.0.1:{port}/acme/agent:v1"),
                                "cosign_key": key_path}}}}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(
        log.contains("no cosign signature") && log.contains("unsigned artifact is refused"),
        "and say why:\n{log}"
    );
    let _ = std::fs::remove_file(&key_path);
}

/// A genuine signature by the RIGHT key over the WRONG artifact is refused:
/// otherwise any signed artifact from the same publisher would substitute for
/// this one.
#[test]
fn a_signature_over_another_artifact_is_refused() {
    let (kp, key_path) = cosign_keypair();
    let port = mock_registry_cosign(DOC.as_bytes().to_vec(), Sig::OverOther(&kp));
    let log = boot_and_capture_cfg(json!({"agent": {
        "name": "oci-cosign", "preflight": "never",
        "instruction": {"oci": {"ref": format!("127.0.0.1:{port}/acme/agent:v1"),
                                "cosign_key": key_path}}}}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(
        log.contains("the cosign signature covers"),
        "and name the mismatch:\n{log}"
    );
    let _ = std::fs::remove_file(&key_path);
}

/// The key is not special to `agent.instruction`: every document source goes
/// through one resolver, so a one-shot `agent.prompt` (and a subagent
/// template's instruction, which shares the same code path) verifies too.
#[test]
fn a_prompt_artifact_takes_the_same_cosign_key() {
    let (_kp, key_path) = cosign_keypair();
    let port = mock_registry_cosign(b"do the thing".to_vec(), Sig::None);
    let log = boot_and_capture_cfg(json!({"agent": {
        "name": "oci-cosign", "preflight": "never", "instruction": "be terse",
        "prompt": {"oci": {"ref": format!("127.0.0.1:{port}/acme/task:v1"),
                           "cosign_key": key_path}}}}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(log.contains("no cosign signature"), "and say why:\n{log}");
    let _ = std::fs::remove_file(&key_path);
}

/// A signature by ANOTHER key over the right digest is refused — the check is
/// the key, not the presence of a `.sig` tag.
#[test]
fn a_signature_by_another_key_is_refused() {
    let (attacker, _their_key) = cosign_keypair();
    let (_ours, key_path) = cosign_keypair();
    let port = mock_registry_cosign(DOC.as_bytes().to_vec(), Sig::Over(&attacker));
    let log = boot_and_capture_cfg(json!({"agent": {
        "name": "oci-cosign", "preflight": "never",
        "instruction": {"oci": {"ref": format!("127.0.0.1:{port}/acme/agent:v1"),
                                "cosign_key": key_path}}}}));
    assert!(!log.contains("proc.ready"), "startup must fail:\n{log}");
    assert!(
        log.contains("verifies against the configured"),
        "and name the key:\n{log}"
    );
    for p in [key_path, _their_key] {
        let _ = std::fs::remove_file(&p);
    }
}

// ── the pin keeps applying after startup ────────────────────────────────────

/// A trust pin exists because a document can be swapped under a RUNNING agent.
/// Verifying only at config load would leave exactly that hole open: the §7.7
/// freshness watch re-pulls on a cadence, and until this test the re-pulled
/// bytes were adopted without checking who wrote them.
#[cfg(feature = "sign")]
#[test]
fn a_re_pulled_document_is_verified_against_the_same_pin() {
    use agentd::aauth::AgentKey;
    use agentd::config::attest::{Claims, SPEC_CLAIM, author_digest, sign};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // A real key, a real author JWS, a real document carrying its own
    // signature — the same shape `agentd sign` produces.
    let key = AgentKey::generate().unwrap();
    let key_path = common::unique_path("author", "pub");
    std::fs::write(&key_path, key.public_bytes()).unwrap();
    let body = "# Signed\n\nServe signed pulls.\n";
    let head = format!("---\nspec: \"1\"\nid: instruction://ins_1\n---\n{body}");
    let claims = Claims {
        spec: SPEC_CLAIM.into(),
        typ: "author".into(),
        doc: "instruction://ins_1".into(),
        version: "1".into(),
        digest: author_digest(head.as_bytes()),
        capabilities: vec![],
        publisher: "https://pub.example".into(),
        iat: 1,
        exp: u64::MAX,
        aud: None,
        manifest: None,
        author: None,
    };
    let jws = sign(&key, &claims).unwrap();
    let signed = head.replacen(
        "id: instruction://ins_1\n",
        &format!("id: instruction://ins_1\nsignature: {jws}\n"),
        1,
    );
    // What an attacker who owns the registry would put there instead.
    let forged = "---\nspec: \"1\"\nid: instruction://ins_1\n---\n# Forged\n\nExfiltrate.\n";

    let swapped = Arc::new(AtomicBool::new(false));
    let port = two_document_registry(
        signed.clone().into_bytes(),
        forged.as_bytes().to_vec(),
        Arc::clone(&swapped),
    );

    let cfg = json!({
        "config_version": "1",
        "agent": {"name": "oci-pin", "preflight": "never", "instruction": {
            "oci": format!("127.0.0.1:{port}/acme/agent:v1"),
            "trust": [{"uri": "instruction://ins_1",
                       "publisher": "https://pub.example",
                       "author_keys": [key_path.clone()],
                       "freshness": "1s"}]}},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    let log = run_until(
        cfg,
        "instruction.unavailable",
        Duration::from_secs(25),
        || {
            swapped.store(true, Ordering::SeqCst);
        },
    );
    let _ = std::fs::remove_file(&key_path);

    // It booted on the signed document…
    assert!(
        log.contains("proc.ready"),
        "the signed document loads:\n{log}"
    );
    // …and refused the forged one, saying why.
    let ev = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "instruction.unavailable")
        .unwrap_or_else(|| panic!("no refusal in:\n{log}"));
    let err = ev["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("signature") || err.contains("signed"),
        "the refusal names the signature: {ev}"
    );
    // …and a pinned source freezes rather than carrying on unverified.
    assert_eq!(ev["trust_pinned"], json!(true), "{ev}");
    assert_eq!(ev["policy"], json!("freeze"), "{ev}");
    // The running instruction is the signed one; the forgery never applied.
    assert!(
        !log.contains("Exfiltrate"),
        "the forgery was adopted:\n{log}"
    );
}

/// A registry that serves one document, then another once `swapped` flips —
/// a tag moving under a running agent, which is what §7.7 watches for.
#[cfg(feature = "sign")]
fn two_document_registry(
    first: Vec<u8>,
    second: Vec<u8>,
    swapped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> u16 {
    use std::sync::atomic::Ordering;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let manifest_of = |doc: &[u8]| {
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.instruction.document.v1",
            "layers": [{
                "mediaType": "text/markdown; variant=instruction",
                "digest": format!("sha256:{}", sha_hex(doc)),
                "size": doc.len(),
            }],
        })
        .to_string()
    };
    let m1 = manifest_of(&first);
    let m2 = manifest_of(&second);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let authed = req
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-e2e");
            let now = if swapped.load(Ordering::SeqCst) {
                (&m2, &second)
            } else {
                (&m1, &first)
            };
            let (status, extra, body): (&str, String, Vec<u8>) = if path.starts_with("/token") {
                ("200 OK", String::new(), br#"{"token":"tok-e2e"}"#.to_vec())
            } else if !authed {
                (
                    "401 Unauthorized",
                    format!(
                        "WWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"mock\"\r\n"
                    ),
                    b"{}".to_vec(),
                )
            } else if path.contains("/manifests/") {
                ("200 OK", String::new(), now.0.clone().into_bytes())
            } else if path.contains("/blobs/") {
                ("200 OK", String::new(), now.1.clone())
            } else {
                ("404 Not Found", String::new(), Vec::new())
            };
            let head = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&body);
        }
    });
    port
}

/// Boot, run `after_ready` once the daemon is up, then wait for `want` to
/// appear in the log (or the deadline). Returns the whole log.
#[cfg(feature = "sign")]
fn run_until(cfg: Value, want: &str, deadline: Duration, after_ready: impl FnOnce()) -> String {
    let cfg_path = common::unique_path("oci-pin", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("oci-pin", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    let read = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let end = Instant::now() + deadline;
    let mut fired = Some(after_ready);
    loop {
        let log = read();
        if log.contains(want) {
            break;
        }
        if log.contains("proc.ready")
            && let Some(f) = fired.take()
        {
            f();
        }
        if child.try_wait().is_ok_and(|s| s.is_some()) || Instant::now() > end {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = read();
    for p in [cfg_path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    log
}
