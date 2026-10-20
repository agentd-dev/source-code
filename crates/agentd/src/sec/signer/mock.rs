// SPDX-License-Identifier: AGPL-3.0-only
//! The reference external signer, for tests and the operators writing
//! their own: every op of the protocol ([`super`]), over a unix socket, with
//! keys it makes at start and never writes anywhere. Hidden mode: `agentd
//! --internal-mock-signer <socket> [--knobs <json>]`.
//!
//! It serves only the client uid it is told to (`allow_uid`; default its
//! own) — the check a real signer makes too, since the socket's directory
//! permissions are the only other thing between it and any local process.
//!
//! Knobs:
//!
//! - `keys`: `{<id>: {"alg": "ES256"} | {"alg": "RS256", "pem_file": …} |
//!   {"use": "wrap"}}` — a P-256 signing key made at start, an RSA key read
//!   from a PKCS#8 file, an AES-256-GCM key-encryption key made at start
//!   (`"hex"` gives its 32 bytes instead). Default `idp` and `ras` (ES256)
//!   and `seal` (wrap).
//! - `record`: a file each request is appended to as `{"op", "key"}` — never
//!   its data.
//! - `allow_uid`: the client uid served.
//! - `outage`: `{"file", "after", "refuse"}` — while `file` exists, the next
//!   request `after` names (`"<op>:<key>"`) is answered and then the
//!   `refuse` requests that follow it are refused (`internal`), as a signer
//!   that fails mid-call does; the file is removed when it arms.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair, RsaKeyPair};
use serde_json::{Value, json};

use super::{peer_uid, read_frame, write_frame};
use crate::auth::seal::SealKey;
use crate::b64;

/// The keys and the record a test starts with.
pub fn default_knobs() -> Value {
    json!({
        "keys": {"idp": {"alg": "ES256"}, "ras": {"alg": "ES256"}, "seal": {"use": "wrap"}},
        "record": null,
        "allow_uid": null,
        "outage": null,
    })
}

enum Key {
    Es256(Box<EcdsaKeyPair>),
    Rs256(Box<RsaKeyPair>),
    Wrap(Box<SealKey>),
}

struct State {
    keys: BTreeMap<String, Key>,
    record: Option<String>,
    allow_uid: u32,
    outage: Value,
    /// Requests still to refuse in an outage.
    refusing: u64,
}

/// A running signer.
pub struct Mock {
    pub socket: std::path::PathBuf,
}

fn keys_of(knobs: &Value) -> Result<BTreeMap<String, Key>, String> {
    let rng = ring::rand::SystemRandom::new();
    let mut out = BTreeMap::new();
    for (id, k) in knobs["keys"].as_object().into_iter().flatten() {
        let key = if k["use"] == "wrap" {
            let raw = match k["hex"].as_str() {
                Some(h) => (0..h.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(h.get(i..i + 2).unwrap_or("zz"), 16))
                    .collect::<Result<Vec<u8>, _>>()
                    .map_err(|_| format!("key {id}: hex"))?,
                None => {
                    let mut b = vec![0u8; 32];
                    crate::sec::random::fill(&mut b).map_err(|e| e.to_string())?;
                    b
                }
            };
            Key::Wrap(Box::new(SealKey::new(&raw)?))
        } else if k["alg"] == "RS256" {
            let pem = std::fs::read_to_string(k["pem_file"].as_str().unwrap_or_default())
                .map_err(|e| format!("key {id}: {e}"))?;
            let body: String = pem
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>();
            let der = b64::std_decode(&body)?;
            Key::Rs256(Box::new(
                RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("key {id}: {e}"))?,
            ))
        } else {
            let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                .map_err(|_| format!("key {id}: generation"))?;
            Key::Es256(Box::new(
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, doc.as_ref(), &rng)
                    .map_err(|e| format!("key {id}: {e}"))?,
            ))
        };
        out.insert(id.clone(), key);
    }
    Ok(out)
}

/// Bind `socket` and answer on a thread of its own until the process ends.
pub fn serve(socket: &Path, knobs: &Value) -> Result<Mock, String> {
    let state = Arc::new(Mutex::new(State {
        keys: keys_of(knobs)?,
        record: knobs["record"].as_str().map(str::to_string),
        // SAFETY: getuid has no failure mode.
        allow_uid: knobs["allow_uid"]
            .as_u64()
            .map_or_else(|| unsafe { libc::getuid() }, |u| u as u32),
        outage: knobs["outage"].clone(),
        refusing: 0,
    }));
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    std::thread::Builder::new()
        .name("mock-signer".into())
        .spawn(move || {
            for c in listener.incoming().flatten() {
                let state = Arc::clone(&state);
                let _ = std::thread::Builder::new()
                    .name("mock-signer-conn".into())
                    .spawn(move || connection(c, &state));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Mock {
        socket: socket.to_path_buf(),
    })
}

/// `--internal-mock-signer`: serve until killed.
pub fn run(socket: &str, knobs: &Value) -> i32 {
    let mut k = default_knobs();
    if let (Some(base), Some(over)) = (k.as_object_mut(), knobs.as_object()) {
        for (key, v) in over {
            base.insert(key.clone(), v.clone());
        }
    }
    match serve(Path::new(socket), &k) {
        Ok(_) => loop {
            std::thread::park();
        },
        Err(e) => {
            eprintln!("--internal-mock-signer: {e}");
            crate::exit::USAGE
        }
    }
}

fn connection(mut c: UnixStream, state: &Arc<Mutex<State>>) {
    let allow = state.lock().unwrap_or_else(|e| e.into_inner()).allow_uid;
    if peer_uid(&c).ok() != Some(allow) {
        return;
    }
    while let Ok(body) = read_frame(&mut c) {
        let answer = match serde_json::from_slice::<Value>(&body) {
            Ok(req) => answer(&req, state),
            Err(_) => refuse("bad_request", "not JSON"),
        };
        if write_frame(&mut c, answer.to_string().as_bytes()).is_err() {
            return;
        }
    }
}

fn refuse(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn field(req: &Value, f: &str) -> Option<Vec<u8>> {
    req[f].as_str().and_then(|s| b64::url_decode(s).ok())
}

fn answer(req: &Value, state: &Arc<Mutex<State>>) -> Value {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let op = req["op"].as_str().unwrap_or_default();
    let id = req["key"].as_str().unwrap_or_default();
    if let Some(path) = &s.record
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{}", json!({"op": op, "key": id}));
    }
    if s.refusing > 0 {
        s.refusing -= 1;
        return refuse("internal", "an outage (the `outage` knob)");
    }
    let out = answer_of(req, &s);
    if let Some(file) = s.outage["file"].as_str()
        && s.outage["after"].as_str() == Some(&format!("{op}:{id}"))
        && std::fs::remove_file(file).is_ok()
    {
        s.refusing = s.outage["refuse"].as_u64().unwrap_or(1);
    }
    out
}

fn answer_of(req: &Value, s: &State) -> Value {
    let op = req["op"].as_str().unwrap_or_default();
    let id = req["key"].as_str().unwrap_or_default();
    let Some(key) = s.keys.get(id) else {
        return refuse("unknown_key", &format!("no key {id:?}"));
    };
    let ok = |mut v: Value| {
        v["ok"] = json!(true);
        v
    };
    if op == "sign" && !matches!(key, Key::Wrap(_)) && req["alg"] != alg_of(key) {
        return refuse("wrong_alg", &format!("{id} is {}", alg_of(key)));
    }
    let rng = ring::rand::SystemRandom::new();
    match (op, key) {
        ("public_key", Key::Es256(p)) => {
            match crate::auth::jwk::p256_public(p.public_key().as_ref()) {
                Ok(jwk) => ok(json!({"alg": "ES256", "jwk": jwk})),
                Err(e) => refuse("internal", &e),
            }
        }
        ("public_key", Key::Rs256(p)) => {
            let parts = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(p.public());
            ok(json!({"alg": "RS256", "jwk": {"kty": "RSA",
                "n": b64::url_nopad(&parts.n), "e": b64::url_nopad(&parts.e)}}))
        }
        ("sign", Key::Es256(p)) => match field(req, "data") {
            Some(data) => match p.sign(&rng, &data) {
                Ok(sig) => ok(json!({"sig": b64::url_nopad(sig.as_ref())})),
                Err(_) => refuse("internal", "signing failed"),
            },
            None => refuse("bad_request", "no base64url data"),
        },
        ("sign", Key::Rs256(p)) => match field(req, "data") {
            Some(data) => {
                let mut sig = vec![0u8; p.public().modulus_len()];
                match p.sign(&ring::signature::RSA_PKCS1_SHA256, &rng, &data, &mut sig) {
                    Ok(()) => ok(json!({"sig": b64::url_nopad(&sig)})),
                    Err(_) => refuse("internal", "signing failed"),
                }
            }
            None => refuse("bad_request", "no base64url data"),
        },
        ("wrap", Key::Wrap(k)) => match (field(req, "plaintext"), field(req, "aad")) {
            (Some(p), Some(a)) => match k.seal(&a, &p) {
                Ok(w) => ok(json!({"wrapped": b64::url_nopad(&w)})),
                Err(e) => refuse("internal", &e),
            },
            _ => refuse("bad_request", "plaintext and aad are base64url"),
        },
        ("unwrap", Key::Wrap(k)) => match (field(req, "wrapped"), field(req, "aad")) {
            (Some(w), Some(a)) => match k.open(&a, &w) {
                Ok(p) => ok(json!({"plaintext": b64::url_nopad(&p)})),
                Err(_) => refuse(
                    "unwrap_failed",
                    "the wrapped key does not open under this key and aad",
                ),
            },
            _ => refuse("bad_request", "wrapped and aad are base64url"),
        },
        ("public_key" | "sign" | "wrap" | "unwrap", _) => {
            refuse("wrong_use", &format!("{id} is not a key for {op}"))
        }
        _ => refuse("bad_request", &format!("no op {op:?}")),
    }
}

fn alg_of(k: &Key) -> &'static str {
    match k {
        Key::Es256(_) => "ES256",
        Key::Rs256(_) => "RS256",
        Key::Wrap(_) => "A256GCM",
    }
}
