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
//! **What it signs** is held to the policy `docs/external-signer.md` asks
//! of every signer ([`assertion`]): only a client assertion — a JWS whose
//! header is `{alg, typ: "JWT", kid}` with the key's own algorithm and id,
//! and whose claims are `iss` = `sub`, an `aud` among the key's
//! `audiences`, an `iat` near its clock, an `exp` at most five minutes
//! after it and a `jti`, and nothing else. Anything else is `denied`, and
//! so is every request for a key with no `audiences`. A wrap key wraps and
//! unwraps and a derivation key derives, each nothing else (`wrong_use`).
//!
//! Knobs:
//!
//! - `keys`: `{<id>: {"alg": "ES256", "audiences": [...]} | {"alg":
//!   "RS256", "pem_file": …, "audiences": [...]} | {"use": "wrap"} |
//!   {"use": "derive"}}` — a P-256 signing key made at start, an RSA key read
//!   from a PKCS#8 file, an AES-256-GCM key-encryption key or an HMAC-SHA256
//!   derivation key made at start (`"hex"` gives its 32 bytes instead); a
//!   signing key's `clients`, when given, are the only `iss` it signs for.
//!   Default `idp` and `ras` (ES256, no audience: they sign nothing until
//!   one is given), `seal` (wrap) and `derive` (derive). With `x402`,
//!   `{"alg": "ES256K", "assets": [...], "payees": [...], "max_value": …,
//!   "max_validity": …}` is a secp256k1 wallet key (`"hex"` its 32 bytes),
//!   which signs only the EIP-3009 typed data its policy allows ([`transfer`]);
//!   `{"alg": "EdDSA", "assets": [...], "payees": [...], "max_value": …}` is
//!   a Solana wallet key (`"hex"` its 32-byte seed), which signs only the
//!   transaction message of an SPL transfer its policy allows
//!   ([`solana_transfer`]).
//! - `record`: a file each request is appended to as `{"op", "key"}`, and
//!   each refusal of the policy as `{"op", "key", "denied"}` — never its
//!   data.
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
use serde_json::{Map, Value, json};

use super::{peer_uid, read_frame, write_frame};
use crate::auth::seal::SealKey;
use crate::b64;

/// The longest an assertion it signs may live: `exp` after `iat`.
pub const MAX_LIFETIME_S: u64 = 300;
/// How far an assertion's `iat` may be from its clock.
pub const MAX_SKEW_S: u64 = 120;

/// The keys and the record a test starts with.
pub fn default_knobs() -> Value {
    json!({
        "keys": {"idp": {"alg": "ES256"}, "ras": {"alg": "ES256"},
                 "seal": {"use": "wrap"}, "derive": {"use": "derive"}},
        "record": null,
        "allow_uid": null,
        "outage": null,
    })
}

enum Kind {
    Es256(Box<EcdsaKeyPair>),
    Rs256(Box<RsaKeyPair>),
    Wrap(Box<SealKey>),
    Derive(Box<[u8; 32]>),
    /// A secp256k1 wallet key (RFC 0049 D8): it signs EIP-3009 typed data.
    #[cfg(feature = "x402")]
    Es256k(Box<k256::ecdsa::SigningKey>),
    /// A Solana wallet key (RFC 0049 §10): it signs an SPL transfer's
    /// transaction message.
    #[cfg(feature = "x402")]
    Ed25519(Box<ring::signature::Ed25519KeyPair>),
}

/// One key and what it may be used for.
pub struct Key {
    kind: Kind,
    /// The `aud` values a signing key signs an assertion for.
    audiences: Vec<String>,
    /// The `iss` values it signs for; empty, any.
    clients: Vec<String>,
    /// A wallet key's policy, as its knob gives it: `assets`, `payees`,
    /// `max_value`, `max_validity`.
    #[cfg_attr(not(feature = "x402"), allow(dead_code))]
    wallet: Value,
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

/// 32 bytes: `hex` when the knob gives them, else random.
fn raw_key(id: &str, k: &Value) -> Result<[u8; 32], String> {
    let mut b = [0u8; 32];
    match k["hex"].as_str() {
        Some(h) => {
            let v = (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(h.get(i..i + 2).unwrap_or("zz"), 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| format!("key {id}: hex"))?;
            b = v
                .try_into()
                .map_err(|_| format!("key {id}: not 32 bytes"))?;
        }
        None => crate::sec::random::fill(&mut b).map_err(|e| e.to_string())?,
    }
    Ok(b)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn keys_of(knobs: &Value) -> Result<BTreeMap<String, Key>, String> {
    let rng = ring::rand::SystemRandom::new();
    let mut out = BTreeMap::new();
    for (id, k) in knobs["keys"].as_object().into_iter().flatten() {
        let kind = if k["use"] == "wrap" {
            Kind::Wrap(Box::new(SealKey::new(&raw_key(id, k)?)?))
        } else if k["use"] == "derive" {
            Kind::Derive(Box::new(raw_key(id, k)?))
        } else if k["alg"] == "ES256K" {
            #[cfg(feature = "x402")]
            {
                Kind::Es256k(Box::new(crate::payments::evm::key_from_bytes(&raw_key(
                    id, k,
                )?)?))
            }
            #[cfg(not(feature = "x402"))]
            {
                return Err(format!("key {id}: ES256K needs the x402 feature"));
            }
        } else if k["alg"] == "EdDSA" {
            #[cfg(feature = "x402")]
            {
                Kind::Ed25519(Box::new(crate::payments::svm::key_pair(&raw_key(id, k)?)?))
            }
            #[cfg(not(feature = "x402"))]
            {
                return Err(format!("key {id}: EdDSA needs the x402 feature"));
            }
        } else if k["alg"] == "RS256" {
            let pem = std::fs::read_to_string(k["pem_file"].as_str().unwrap_or_default())
                .map_err(|e| format!("key {id}: {e}"))?;
            let body: String = pem
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>();
            let der = b64::std_decode(&body)?;
            Kind::Rs256(Box::new(
                RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("key {id}: {e}"))?,
            ))
        } else {
            let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                .map_err(|_| format!("key {id}: generation"))?;
            Kind::Es256(Box::new(
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, doc.as_ref(), &rng)
                    .map_err(|e| format!("key {id}: {e}"))?,
            ))
        };
        out.insert(
            id.clone(),
            Key {
                kind,
                audiences: strings(&k["audiences"]),
                clients: strings(&k["clients"]),
                wallet: k.clone(),
            },
        );
    }
    Ok(out)
}

/// Whether `data`, asked to be signed with `key` (id `id`) under `alg`, is
/// a client assertion this key signs at `now_s`: the JWS signing input of
/// a header `{alg, typ: "JWT", kid}` naming this key and its algorithm, and
/// of the claims `iss` = `sub` (one of the key's `clients`, when it has
/// them), an `aud` among its audiences, an `iat` within [`MAX_SKEW_S`] of
/// now, an `exp` after it by at most [`MAX_LIFETIME_S`], a `jti` of 16 to
/// 128 base64url characters, and no other member. `Err` says why not —
/// never with what the data held.
fn assertion(data: &[u8], id: &str, alg: &str, key: &Key, now_s: u64) -> Result<(), String> {
    if key.audiences.is_empty() {
        return Err(format!(
            "key {id} has no audience configured, so it signs nothing"
        ));
    }
    let text =
        std::str::from_utf8(data).map_err(|_| "the signing input is not text".to_string())?;
    let (h, c) = text
        .split_once('.')
        .filter(|(_, c)| !c.contains('.'))
        .ok_or("the signing input is not a JWS header.claims")?;
    let object = |part: &str, what: &str| -> Result<Map<String, Value>, String> {
        b64::url_decode(part)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_object().cloned())
            .ok_or(format!("its {what} is not a base64url JSON object"))
    };
    let only = |m: &Map<String, Value>, keys: &[&str], what: &str| {
        let mut have: Vec<&str> = m.keys().map(String::as_str).collect();
        have.sort_unstable();
        let mut want = keys.to_vec();
        want.sort_unstable();
        if have == want {
            Ok(())
        } else {
            Err(format!(
                "its {what} members are not exactly {}",
                keys.join(", ")
            ))
        }
    };
    let header = object(h, "header")?;
    only(&header, &["alg", "typ", "kid"], "header")?;
    if header["alg"] != alg || header["typ"] != "JWT" || header["kid"] != id {
        return Err(format!(
            "its header is not {{alg: {alg}, typ: JWT, kid: {id}}}"
        ));
    }
    let claims = object(c, "claims")?;
    only(
        &claims,
        &["iss", "sub", "aud", "iat", "exp", "jti"],
        "claims",
    )?;
    let iss = claims["iss"].as_str().unwrap_or_default();
    if iss.is_empty() || iss.len() > 256 || claims["sub"] != iss {
        return Err("its iss and sub are not one client id".into());
    }
    if !key.clients.is_empty() && !key.clients.iter().any(|c| c == iss) {
        return Err(format!("key {id} does not sign for that client"));
    }
    if !claims["aud"]
        .as_str()
        .is_some_and(|a| key.audiences.iter().any(|x| x == a))
    {
        return Err(format!("its aud is not an audience key {id} signs for"));
    }
    let (Some(iat), Some(exp)) = (claims["iat"].as_u64(), claims["exp"].as_u64()) else {
        return Err("its iat and exp are not whole seconds".into());
    };
    if iat.abs_diff(now_s) > MAX_SKEW_S || exp <= iat || exp - iat > MAX_LIFETIME_S {
        return Err(format!(
            "its iat is not within {MAX_SKEW_S}s of now, or its exp not within {MAX_LIFETIME_S}s after it"
        ));
    }
    let jti = claims["jti"].as_str().unwrap_or_default();
    if !(16..=128).contains(&jti.len())
        || !jti
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("its jti is not 16 to 128 base64url characters".into());
    }
    Ok(())
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

/// Append `line` to the request record, when there is one.
fn record(s: &State, line: Value) {
    if let Some(path) = &s.record
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn answer(req: &Value, state: &Arc<Mutex<State>>) -> Value {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    let op = req["op"].as_str().unwrap_or_default();
    let id = req["key"].as_str().unwrap_or_default();
    record(&s, json!({"op": op, "key": id}));
    if s.refusing > 0 {
        s.refusing -= 1;
        return refuse("internal", "an outage (the `outage` knob)");
    }
    let out = answer_of(req, &s);
    if out["error"]["code"] == "denied" {
        record(
            &s,
            json!({"op": op, "key": id, "denied": out["error"]["message"]}),
        );
    }
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
    #[cfg(feature = "x402")]
    if let Kind::Es256k(sk) = &key.kind {
        let now = crate::auth::cache::now_ms() / 1000;
        return match op {
            "public_key" => ok(json!({"alg": "ES256K",
                "jwk": crate::payments::evm::jwk_of(sk.verifying_key())})),
            "sign_typed_data" => match transfer(&req["typed_data"], id, key, sk, now) {
                Ok(digest) => ok(json!({"sig": b64::url_nopad(
                    &crate::payments::evm::sign_digest(sk, &digest))})),
                Err(why) => refuse(
                    "denied",
                    &format!("not an EIP-3009 authorization this signer signs: {why}"),
                ),
            },
            "sign" | "wrap" | "unwrap" | "derive" | "sign_solana_transaction" => {
                refuse("wrong_use", &format!("{id} is not a key for {op}"))
            }
            _ => refuse("bad_request", &format!("no op {op:?}")),
        };
    }
    #[cfg(feature = "x402")]
    if let Kind::Ed25519(kp) = &key.kind {
        use crate::payments::svm;
        return match op {
            "public_key" => ok(json!({"alg": "EdDSA", "jwk": svm::jwk_of(&svm::pubkey_of(kp))})),
            "sign_solana_transaction" => match field(req, "message") {
                None => refuse("bad_request", "no base64url message"),
                Some(m) => match solana_transfer(&m, id, key, kp) {
                    Ok(()) => ok(json!({"sig": b64::url_nopad(&svm::sign(kp, &m))})),
                    Err(why) => refuse(
                        "denied",
                        &format!("not an SPL transfer this signer signs: {why}"),
                    ),
                },
            },
            "sign" | "wrap" | "unwrap" | "derive" | "sign_typed_data" => {
                refuse("wrong_use", &format!("{id} is not a key for {op}"))
            }
            _ => refuse("bad_request", &format!("no op {op:?}")),
        };
    }
    #[cfg(feature = "x402")]
    if op == "sign_typed_data" {
        return refuse("wrong_use", &format!("{id} is not an EVM key"));
    }
    #[cfg(feature = "x402")]
    if op == "sign_solana_transaction" {
        return refuse("wrong_use", &format!("{id} is not a Solana key"));
    }
    if op == "sign" && matches!(key.kind, Kind::Es256(_) | Kind::Rs256(_)) {
        if req["alg"] != alg_of(key) {
            return refuse("wrong_alg", &format!("{id} is {}", alg_of(key)));
        }
        let Some(data) = field(req, "data") else {
            return refuse("bad_request", "no base64url data");
        };
        let now = crate::auth::cache::now_ms() / 1000;
        if let Err(why) = assertion(&data, id, alg_of(key), key, now) {
            return refuse(
                "denied",
                &format!("not a client assertion this signer signs: {why}"),
            );
        }
    }
    let rng = ring::rand::SystemRandom::new();
    match (op, &key.kind) {
        ("public_key", Kind::Es256(p)) => {
            match crate::auth::jwk::p256_public(p.public_key().as_ref()) {
                Ok(jwk) => ok(json!({"alg": "ES256", "jwk": jwk})),
                Err(e) => refuse("internal", &e),
            }
        }
        ("public_key", Kind::Rs256(p)) => {
            let parts = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(p.public());
            ok(json!({"alg": "RS256", "jwk": {"kty": "RSA",
                "n": b64::url_nopad(&parts.n), "e": b64::url_nopad(&parts.e)}}))
        }
        ("sign", Kind::Es256(p)) => match field(req, "data") {
            Some(data) => match p.sign(&rng, &data) {
                Ok(sig) => ok(json!({"sig": b64::url_nopad(sig.as_ref())})),
                Err(_) => refuse("internal", "signing failed"),
            },
            None => refuse("bad_request", "no base64url data"),
        },
        ("sign", Kind::Rs256(p)) => match field(req, "data") {
            Some(data) => {
                let mut sig = vec![0u8; p.public().modulus_len()];
                match p.sign(&ring::signature::RSA_PKCS1_SHA256, &rng, &data, &mut sig) {
                    Ok(()) => ok(json!({"sig": b64::url_nopad(&sig)})),
                    Err(_) => refuse("internal", "signing failed"),
                }
            }
            None => refuse("bad_request", "no base64url data"),
        },
        ("wrap", Kind::Wrap(k)) => match (field(req, "plaintext"), field(req, "aad")) {
            (Some(p), Some(a)) => match k.seal(&a, &p) {
                Ok(w) => ok(json!({"wrapped": b64::url_nopad(&w)})),
                Err(e) => refuse("internal", &e),
            },
            _ => refuse("bad_request", "plaintext and aad are base64url"),
        },
        ("unwrap", Kind::Wrap(k)) => match (field(req, "wrapped"), field(req, "aad")) {
            (Some(w), Some(a)) => match k.open(&a, &w) {
                Ok(p) => ok(json!({"plaintext": b64::url_nopad(&p)})),
                Err(_) => refuse(
                    "unwrap_failed",
                    "the wrapped key does not open under this key and aad",
                ),
            },
            _ => refuse("bad_request", "wrapped and aad are base64url"),
        },
        ("derive", Kind::Derive(k)) => match field(req, "data") {
            Some(d) if !d.is_empty() && d.len() <= 256 => {
                ok(json!({"mac": b64::url_nopad(&crate::sha::hmac_sha256(&k[..], &d))}))
            }
            _ => refuse("bad_request", "data is 1 to 256 base64url bytes"),
        },
        ("public_key" | "sign" | "wrap" | "unwrap" | "derive", _) => {
            refuse("wrong_use", &format!("{id} is not a key for {op}"))
        }
        _ => refuse("bad_request", &format!("no op {op:?}")),
    }
}

fn alg_of(k: &Key) -> &'static str {
    match k.kind {
        Kind::Es256(_) => "ES256",
        Kind::Rs256(_) => "RS256",
        Kind::Wrap(_) => "A256GCM",
        Kind::Derive(_) => "HS256",
        #[cfg(feature = "x402")]
        Kind::Es256k(_) => "ES256K",
        #[cfg(feature = "x402")]
        Kind::Ed25519(_) => "EdDSA",
    }
}

/// The digest of `td` when it is an EIP-3009 authorization the wallet key
/// `key` (id `id`) signs at `now_s`, held to the policy
/// `docs/external-signer.md` asks of a signer: exactly EIP-3009's typed
/// data, for a (chain, token) among the key's `assets`, `from` the key's own
/// address, `to` among its `payees` when it has them, `value` at most its
/// `max_value`, a `validBefore` after now and at most `max_validity`
/// seconds (default 3600) away, and a 32-byte nonce. A key with no
/// `assets` signs nothing. `Err` says why not — never with what it held.
#[cfg(feature = "x402")]
fn transfer(
    td: &Value,
    id: &str,
    key: &Key,
    sk: &k256::ecdsa::SigningKey,
    now_s: u64,
) -> Result<[u8; 32], String> {
    use crate::payments::{caip::AssetId, evm, x402::Transfer};
    let assets: Vec<(u64, evm::Address)> = strings(&key.wallet["assets"])
        .iter()
        .filter_map(|a| AssetId::parse(a).ok()?.erc20())
        .collect();
    if assets.is_empty() {
        return Err(format!(
            "key {id} has no asset configured, so it signs nothing"
        ));
    }
    let t = Transfer::read(td)?;
    if !assets.contains(&(t.chain_id, t.contract)) {
        return Err(format!("the token is not an asset key {id} signs for"));
    }
    let a = &t.authorization;
    if a.from != evm::address_of_key(sk) {
        return Err(format!("from is not key {id}'s address"));
    }
    let payees: Vec<evm::Address> = strings(&key.wallet["payees"])
        .iter()
        .filter_map(|p| evm::Address::parse(p).ok())
        .collect();
    if !payees.is_empty() && !payees.contains(&a.to) {
        return Err(format!("to is not a payee key {id} signs for"));
    }
    if let Some(max) = key.wallet["max_value"]
        .as_str()
        .and_then(|m| m.parse::<u128>().ok())
        && a.value > max
    {
        return Err(format!("value is more than key {id} signs for"));
    }
    let window = key.wallet["max_validity"].as_u64().unwrap_or(3600);
    if a.valid_before <= now_s || a.valid_before - now_s > window || a.valid_after >= a.valid_before
    {
        return Err(format!(
            "validBefore is not after now and within {window}s of it, or validAfter is not before it"
        ));
    }
    crate::payments::eip712::digest(td)
}

/// Whether `message` is the transaction message of an SPL transfer the
/// Solana wallet key `key` (id `id`) signs, held to the policy
/// `docs/external-signer.md` asks of a signer: exactly x402's exact
/// transfer (`payments::svm::Transfer::read` — the fee payer in no
/// instruction, the source the key's own token account), the key its
/// authority, of a mint among the key's `assets`, paying one of its `payees`
/// when it has them, at most its `max_value`. A key with no `assets` signs
/// nothing. `Err` says why not — never with what it held.
#[cfg(feature = "x402")]
fn solana_transfer(
    message: &[u8],
    id: &str,
    key: &Key,
    kp: &ring::signature::Ed25519KeyPair,
) -> Result<(), String> {
    use crate::payments::{caip::AssetId, svm};
    let mints: Vec<svm::Pubkey> = strings(&key.wallet["assets"])
        .iter()
        .filter_map(|a| svm::mint_of(&AssetId::parse(a).ok()?))
        .collect();
    if mints.is_empty() {
        return Err(format!(
            "key {id} has no asset configured, so it signs nothing"
        ));
    }
    let t = svm::Transfer::read(&svm::Message::parse(message)?)?;
    if t.owner != svm::pubkey_of(kp) {
        return Err(format!(
            "the transfer's authority is not key {id}'s address"
        ));
    }
    if !mints.contains(&t.mint) {
        return Err(format!("the mint is not an asset key {id} signs for"));
    }
    let payees: Vec<svm::Pubkey> = strings(&key.wallet["payees"])
        .iter()
        .filter_map(|p| svm::Pubkey::parse(p).ok())
        .collect();
    if !payees.is_empty() && !payees.iter().any(|p| t.pays(p)) {
        return Err(format!(
            "the destination is no payee's token account key {id} signs for"
        ));
    }
    if let Some(max) = key.wallet["max_value"]
        .as_str()
        .and_then(|m| m.parse::<u64>().ok())
        && t.amount > max
    {
        return Err(format!("the amount is more than key {id} signs for"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(audiences: &[&str], clients: &[&str]) -> Key {
        Key {
            kind: Kind::Derive(Box::new([0; 32])),
            audiences: audiences.iter().map(|s| s.to_string()).collect(),
            clients: clients.iter().map(|s| s.to_string()).collect(),
            wallet: Value::Null,
        }
    }

    fn input(header: Value, claims: Value) -> Vec<u8> {
        format!(
            "{}.{}",
            b64::url_nopad(header.to_string().as_bytes()),
            b64::url_nopad(claims.to_string().as_bytes())
        )
        .into_bytes()
    }

    /// A wallet key signs only an EIP-3009 authorization its policy allows
    /// (RFC 0049 D8): the typed data agentd builds, for a configured token,
    /// from its own address, to a configured payee, within its value and its
    /// window — and nothing else, whatever the typed data says it is.
    #[cfg(feature = "x402")]
    #[test]
    fn a_wallet_key_signs_only_the_eip3009_shape_its_policy_allows() {
        use crate::payments::evm::{Address, address_of_key, key_from_bytes};
        use crate::payments::x402::Authorization;
        let sk = key_from_bytes(&[0x33; 32]).unwrap();
        let usdc = Address::parse("0x036CbD53842c5426634e7929541eC2318f3dCF7e").unwrap();
        let payee = Address::parse("0x209693Bc6afc0C5328bA36FaF03C514EF312287C").unwrap();
        let k = Key {
            kind: Kind::Derive(Box::new([0; 32])),
            audiences: Vec::new(),
            clients: Vec::new(),
            wallet: json!({"alg": "ES256K", "assets": [format!("eip155:84532/erc20:{usdc:?}")],
                           "payees": [payee.checksummed()], "max_value": "50000", "max_validity": 600}),
        };
        let now = 1_800_000_000;
        let auth = Authorization {
            from: address_of_key(&sk),
            to: payee,
            value: 10_000,
            valid_after: now - 30,
            valid_before: now + 60,
            nonce: [9; 32],
        };
        let td = auth.typed_data("USDC", "2", 84532, &usdc);
        assert_eq!(
            transfer(&td, "w", &k, &sk, now),
            crate::payments::eip712::digest(&td)
        );
        let with = |f: &dyn Fn(&mut Authorization)| {
            let mut a = auth.clone();
            f(&mut a);
            transfer(&a.typed_data("USDC", "2", 84532, &usdc), "w", &k, &sk, now)
        };
        let mut permit = td.clone();
        permit["primaryType"] = json!("Permit");
        let mut extra = td.clone();
        extra["message"]["memo"] = json!("x");
        let mut salted = td.clone();
        salted["domain"]["salt"] = json!(format!("0x{}", "00".repeat(32)));
        for (case, r) in [
            ("another primary type", transfer(&permit, "w", &k, &sk, now)),
            ("a member more", transfer(&extra, "w", &k, &sk, now)),
            ("a domain salt", transfer(&salted, "w", &k, &sk, now)),
            (
                "another chain",
                transfer(
                    &auth.typed_data("USDC", "2", 8453, &usdc),
                    "w",
                    &k,
                    &sk,
                    now,
                ),
            ),
            (
                "another token",
                transfer(
                    &auth.typed_data("USDC", "2", 84532, &payee),
                    "w",
                    &k,
                    &sk,
                    now,
                ),
            ),
            ("someone else's from", with(&|a| a.from = payee)),
            ("another payee", with(&|a| a.to = usdc)),
            ("past max_value", with(&|a| a.value = 50_001)),
            ("expired", with(&|a| a.valid_before = now)),
            ("past its window", with(&|a| a.valid_before = now + 601)),
        ] {
            assert!(r.is_err(), "{case} must be refused");
        }
        let none = Key {
            wallet: json!({"alg": "ES256K"}),
            ..k
        };
        assert!(
            transfer(&td, "w", &none, &sk, now)
                .unwrap_err()
                .contains("signs nothing")
        );
    }

    /// Only the shape of a client assertion, for a configured audience, is
    /// signed: every header and claim agentd sends is read, each limit is
    /// held, and a member past the set is refused.
    #[test]
    fn only_a_client_assertion_for_a_configured_audience_passes() {
        let k = key(&["https://idp.example/token"], &[]);
        let now = 1_800_000_000;
        let header = json!({"alg": "ES256", "typ": "JWT", "kid": "idp"});
        let claims = json!({"iss": "c", "sub": "c", "aud": "https://idp.example/token",
                            "iat": now, "exp": now + 60, "jti": "abcdefghijklmnopqrstuv"});
        assert_eq!(
            assertion(
                &input(header.clone(), claims.clone()),
                "idp",
                "ES256",
                &k,
                now
            ),
            Ok(())
        );
        // The real input agentd builds.
        let real = crate::auth::client_assertion::assertion_input(
            "c",
            "https://idp.example/token",
            "ES256",
            "idp",
        )
        .unwrap();
        let now_s = crate::auth::cache::now_ms() / 1000;
        assert_eq!(
            assertion(real.as_bytes(), "idp", "ES256", &k, now_s),
            Ok(())
        );
        let with = |f: &dyn Fn(&mut Value, &mut Value)| {
            let (mut h, mut c) = (header.clone(), claims.clone());
            f(&mut h, &mut c);
            assertion(&input(h, c), "idp", "ES256", &k, now)
        };
        let refused: Vec<(&str, Result<(), String>)> = vec![
            (
                "not a JWS",
                assertion(b"header.claims", "idp", "ES256", &k, now),
            ),
            ("three parts", assertion(b"a.b.c", "idp", "ES256", &k, now)),
            (
                "another aud",
                with(&|_, c| c["aud"] = json!("https://evil.example/token")),
            ),
            ("another kid", with(&|h, _| h["kid"] = json!("ras"))),
            ("another alg", with(&|h, _| h["alg"] = json!("none"))),
            (
                "no typ",
                with(&|h, _| {
                    h.as_object_mut().unwrap().remove("typ");
                }),
            ),
            (
                "an extra header",
                with(&|h, _| h["jku"] = json!("https://x")),
            ),
            ("an extra claim", with(&|_, c| c["scope"] = json!("admin"))),
            ("sub is not iss", with(&|_, c| c["sub"] = json!("someone"))),
            (
                "a long life",
                with(&|_, c| c["exp"] = json!(now + MAX_LIFETIME_S + 1)),
            ),
            ("exp before iat", with(&|_, c| c["exp"] = json!(now - 1))),
            (
                "an old iat",
                with(&|_, c| {
                    c["iat"] = json!(now - 600);
                    c["exp"] = json!(now - 540);
                }),
            ),
            ("a short jti", with(&|_, c| c["jti"] = json!("x"))),
            (
                "an aud array",
                with(&|_, c| c["aud"] = json!(["https://idp.example/token"])),
            ),
        ];
        for (what, r) in refused {
            assert!(r.is_err(), "{what} must be refused");
        }
        // No audience configured: nothing is signed; a client list holds iss.
        assert!(
            assertion(
                &input(header.clone(), claims.clone()),
                "idp",
                "ES256",
                &key(&[], &[]),
                now
            )
            .is_err()
        );
        let held = key(&["https://idp.example/token"], &["other"]);
        assert!(assertion(&input(header, claims), "idp", "ES256", &held, now).is_err());
    }
}
