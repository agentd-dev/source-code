// SPDX-License-Identifier: AGPL-3.0-only
//! A mock x402 resource server (RFC 0049 §9), for tests and the `payments`
//! conformance family. Hidden mode: `agentd --internal-mock-x402
//! <addr-file> [--knobs <json>]`.
//!
//! One loopback listener, announced in `<addr-file>` as `{"url": …}`. Every
//! path but `/_mock/…` is a resource, paid for as x402 v2 over HTTP says
//! [X-HTTP]:
//!
//! - A request with no `PAYMENT-SIGNATURE` is answered 402 with a
//!   `PAYMENT-REQUIRED`: one `exact` entry of `price` atomic units of
//!   `asset` on `network` to `pay_to`, the token's EIP-712 domain in
//!   `extra` — or the `accepts` a test gives — for `resource.url` the URL
//!   asked, or `resource_url` when a test names another.
//! - A request with one is held to what it was offered: `x402Version` 2,
//!   `accepted` one of the entries offered, exactly; `resource` the one
//!   offered; the authorization's `to` the entry's `payTo` and `value` its
//!   `amount`, `validAfter` past and `validBefore` to come, a 32-byte nonce
//!   it has not seen; and a signature that recovers to `from` over the
//!   EIP-3009 typed data under the entry's token domain — the check an
//!   EIP-3009 token makes on chain. A payment that fails one is answered 402
//!   with a `PAYMENT-RESPONSE` naming x402's error code for it.
//! - A payment that holds is answered as `settle` says: `ok` (200, the
//!   resource, `success: true` with a transaction and the payer), `fail`
//!   (402, `insufficient_funds`, no transaction), `pending` (200,
//!   `settlement_pending` with a transaction), `none` (200, no
//!   `PAYMENT-RESPONSE`), `error` (500, none), `garbled` (200, a
//!   `PAYMENT-RESPONSE` that is not base64 JSON) or `stall` (no answer for
//!   `stall_ms`, then the connection closed).
//!
//! Every request is recorded (`GET /_mock/record`) — its method, path,
//! whether it carried a payment, the header and the payment as sent, what
//! the checks found, and the settlement answered — and a test steers it
//! with `POST /_mock/control`, a JSON object merged into its knobs.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::evm::{self, Address};
use super::x402::{self, Authorization};

/// The headers of [X-HTTP], written here as the transport names them
/// rather than taken from the client's constants: a client that sent its
/// payment under another name would be answered as one that sent none.
const REQUIRED: &str = "PAYMENT-REQUIRED";
const SIGNATURE: &str = "PAYMENT-SIGNATURE";
const RESPONSE: &str = "PAYMENT-RESPONSE";

/// What a test can change, and the defaults: USDC on Base Sepolia, as
/// x402's own examples have it.
pub fn default_knobs() -> Value {
    json!({
        "price": "10000",
        "network": "eip155:84532",
        "asset": "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
        "pay_to": "0x209693Bc6afc0C5328bA36FaF03C514EF312287C",
        "name": "USDC",
        "version": "2",
        "max_timeout": 60,
        "description": "Access to premium market data",
        // The entries offered, when not the one above.
        "accepts": null,
        // The resource the 402 names, when not the URL asked.
        "resource_url": null,
        // Echoed as the PaymentRequired's extensions.
        "extensions": null,
        // ok | fail | pending | none | error | garbled | stall
        "settle": "ok",
        "stall_ms": 10_000,
    })
}

struct State {
    knobs: Value,
    origin: String,
    nonces: BTreeSet<String>,
    record: Vec<Value>,
}

/// The running mock.
pub struct Mock {
    /// Its origin: `http://127.0.0.1:<port>`.
    pub url: String,
    state: Arc<Mutex<State>>,
}

impl Mock {
    /// Merge `knobs` into the mock's.
    pub fn control(&self, knobs: &Value) {
        merge(&mut lock(&self.state).knobs, knobs);
    }

    /// Every request received so far.
    pub fn record(&self) -> Vec<Value> {
        lock(&self.state).record.clone()
    }
}

fn lock(s: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|e| e.into_inner())
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
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut k = default_knobs();
    merge(&mut k, knobs);
    let state = Arc::new(Mutex::new(State {
        knobs: k,
        origin: url.clone(),
        nonces: BTreeSet::new(),
        record: Vec::new(),
    }));
    let s = Arc::clone(&state);
    std::thread::Builder::new()
        .name("mock-x402".into())
        .spawn(move || {
            for c in listener.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || serve(c, &s));
            }
        })?;
    Ok(Mock { url, state })
}

/// `--internal-mock-x402`: start, announce, serve until killed.
pub fn run_with(addr_file: &str, knobs: &Value) -> i32 {
    let mock = match start(knobs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mock-x402: {e}");
            return crate::exit::USAGE;
        }
    };
    let tmp = format!("{addr_file}.tmp");
    let announced = std::fs::write(&tmp, json!({"url": mock.url}).to_string())
        .and_then(|()| std::fs::rename(&tmp, addr_file));
    if let Err(e) = announced {
        eprintln!("mock-x402: announce: {e}");
        return crate::exit::USAGE;
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// One HTTP request.
struct Http {
    method: String,
    target: String,
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
    Some(Http {
        method,
        target,
        headers,
        body,
    })
}

fn write(mut stream: &TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        402 => "Payment Required",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
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

/// The `PaymentRequired` offered for `target`.
fn required(s: &State, target: &str) -> Value {
    let k = &s.knobs;
    let accepts = if k["accepts"].is_array() {
        k["accepts"].clone()
    } else {
        json!([{
            "scheme": "exact", "network": k["network"], "amount": k["price"],
            "asset": k["asset"], "payTo": k["pay_to"], "maxTimeoutSeconds": k["max_timeout"],
            "extra": {"name": k["name"], "version": k["version"]},
        }])
    };
    let url = k["resource_url"]
        .as_str()
        .map_or_else(|| format!("{}{target}", s.origin), str::to_string);
    let mut v = json!({
        "x402Version": x402::VERSION,
        "error": "PAYMENT-SIGNATURE header is required",
        "resource": {"url": url, "description": k["description"], "mimeType": "application/json"},
        "accepts": accepts,
    });
    if !k["extensions"].is_null() {
        v["extensions"] = k["extensions"].clone();
    }
    v
}

fn serve(stream: TcpStream, state: &Arc<Mutex<State>>) {
    let Some(req) = read(&stream) else { return };
    if req.target == "/_mock/record" {
        let rec = json!(lock(state).record);
        write(&stream, 200, &[], rec.to_string().as_bytes());
        return;
    }
    if req.target == "/_mock/control" && req.method == "POST" {
        let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        merge(&mut lock(state).knobs, &v);
        write(&stream, 200, &[], b"{}");
        return;
    }
    let Some(header) = req.header(SIGNATURE) else {
        let offer = {
            let mut s = lock(state);
            s.record
                .push(json!({"method": req.method, "path": req.target, "paid": false}));
            required(&s, &req.target)
        };
        write(
            &stream,
            402,
            &[(REQUIRED, x402::encode_header(&offer))],
            b"{}",
        );
        return;
    };
    let payment = x402::decode_header(header);
    let (offer, settle, stall_ms, network) = {
        let s = lock(state);
        (
            required(&s, &req.target),
            s.knobs["settle"].as_str().unwrap_or("ok").to_string(),
            s.knobs["stall_ms"].as_u64().unwrap_or(10_000),
            s.knobs["network"].clone(),
        )
    };
    let checked = payment
        .as_ref()
        .map_err(|_| "invalid_payload".to_string())
        .and_then(|p| check(p, &offer));
    // A nonce is taken once: a second payment with it is a replay.
    let checked = checked.and_then(|(from, nonce)| {
        if lock(state).nonces.insert(nonce.clone()) {
            Ok((from, nonce))
        } else {
            Err("invalid_payload".into())
        }
    });
    let mut entry = json!({"method": req.method, "path": req.target, "paid": true,
                           "header": header, "payment": payment.as_ref().ok(), "settle": settle});
    let (status, response, body): (u16, Option<String>, Vec<u8>) = match &checked {
        Err(code) => {
            entry["error"] = json!(code);
            let r = json!({"success": false, "errorReason": code, "transaction": "", "network": network});
            (402, Some(x402::encode_header(&r)), b"{}".to_vec())
        }
        Ok((from, nonce)) => {
            entry["valid"] = json!(true);
            entry["from"] = json!(from.checksummed());
            entry["nonce"] = json!(nonce);
            let tx = evm::hex0x(&evm::keccak256(nonce.as_bytes()));
            let resource = json!({"data": "premium market data", "paid_by": from.checksummed()})
                .to_string()
                .into_bytes();
            let ok = json!({"success": true, "transaction": tx, "network": network,
                            "payer": from.checksummed()});
            match settle.as_str() {
                "fail" => (
                    402,
                    Some(x402::encode_header(
                        &json!({"success": false, "errorReason": "insufficient_funds",
                        "transaction": "", "network": network, "payer": from.checksummed()}),
                    )),
                    b"{}".to_vec(),
                ),
                "pending" => (
                    200,
                    Some(x402::encode_header(
                        &json!({"success": false, "errorReason": "settlement_pending",
                        "transaction": tx, "network": network, "payer": from.checksummed()}),
                    )),
                    resource,
                ),
                "none" => (200, None, resource),
                "error" => (500, None, b"{}".to_vec()),
                "garbled" => (200, Some("not a settlement!".into()), resource),
                "stall" => {
                    lock(state).record.push(entry);
                    std::thread::sleep(std::time::Duration::from_millis(stall_ms));
                    return;
                }
                _ => (200, Some(x402::encode_header(&ok)), resource),
            }
        }
    };
    entry["status"] = json!(status);
    entry["response"] = response
        .as_deref()
        .and_then(|r| x402::decode_header(r).ok())
        .unwrap_or(Value::Null);
    lock(state).record.push(entry);
    let headers: Vec<(&str, String)> = response.map(|r| vec![(RESPONSE, r)]).unwrap_or_default();
    write(&stream, status, &headers, &body);
}

/// Hold `p`, a `PaymentPayload`, to the `offer` it answers; the payer and
/// the nonce, or x402's error code for what does not hold [X §9].
fn check(p: &Value, offer: &Value) -> Result<(Address, String), String> {
    if p["x402Version"].as_u64() != Some(x402::VERSION) {
        return Err("invalid_x402_version".into());
    }
    let accepted = &p["accepted"];
    if !offer["accepts"]
        .as_array()
        .is_some_and(|a| a.iter().any(|e| e == accepted))
    {
        return Err("invalid_payment_requirements".into());
    }
    if p["resource"] != offer["resource"] || p.get("extensions") != offer.get("extensions") {
        return Err("invalid_payload".into());
    }
    if accepted["scheme"] != "exact" {
        return Err("invalid_scheme".into());
    }
    let chain_id = super::caip::ChainId::parse(accepted["network"].as_str().unwrap_or_default())
        .ok()
        .and_then(|c| c.evm())
        .ok_or("invalid_network")?;
    let a = &p["payload"]["authorization"];
    let addr = |v: &Value| v.as_str().and_then(|s| Address::parse_any(s).ok());
    let num = |v: &Value| v.as_str().and_then(|s| s.parse::<u128>().ok());
    let (Some(from), Some(to), Some(value), Some(after), Some(before)) = (
        addr(&a["from"]),
        addr(&a["to"]),
        num(&a["value"]),
        num(&a["validAfter"]),
        num(&a["validBefore"]),
    ) else {
        return Err("invalid_payload".into());
    };
    let nonce_hex = a["nonce"].as_str().unwrap_or_default().to_string();
    let nonce = evm::unhex0x(&nonce_hex)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or("invalid_payload")?;
    if Some(to) != addr(&accepted["payTo"]) {
        return Err("invalid_exact_evm_payload_recipient_mismatch".into());
    }
    if Some(value) != num(&accepted["amount"]) {
        return Err("invalid_exact_evm_payload_authorization_value_mismatch".into());
    }
    let now = u128::from(crate::state::now_ms() / 1000);
    if after > now {
        return Err("invalid_exact_evm_payload_authorization_valid_after".into());
    }
    if before <= now {
        return Err("invalid_exact_evm_payload_authorization_valid_before".into());
    }
    let (Ok(valid_after), Ok(valid_before)) = (u64::try_from(after), u64::try_from(before)) else {
        return Err("invalid_payload".into());
    };
    let token = addr(&accepted["asset"]).ok_or("invalid_payment_requirements")?;
    let auth = Authorization {
        from,
        to,
        value,
        valid_after,
        valid_before,
        nonce,
    };
    let td = auth.typed_data(
        accepted["extra"]["name"].as_str().unwrap_or_default(),
        accepted["extra"]["version"].as_str().unwrap_or_default(),
        chain_id,
        &token,
    );
    let digest = super::eip712::digest(&td).map_err(|_| "invalid_payload")?;
    let sig = p["payload"]["signature"]
        .as_str()
        .and_then(|s| evm::unhex0x(s).ok())
        .ok_or("invalid_exact_evm_payload_signature")?;
    match evm::recover(&digest, &sig) {
        Ok(signer) if signer == from => Ok((from, nonce_hex)),
        _ => Err("invalid_exact_evm_payload_signature".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payments::evm::{address_of_key, key_from_bytes, sign_digest};

    /// A payment the way a wallet makes one for `offer`'s first entry.
    fn pay(
        offer: &Value,
        key: &k256::ecdsa::SigningKey,
        nonce: [u8; 32],
        tweak: &dyn Fn(&mut Authorization),
    ) -> Value {
        let e = &offer["accepts"][0];
        let now = crate::state::now_ms() / 1000;
        let mut auth = Authorization {
            from: address_of_key(key),
            to: Address::parse_any(e["payTo"].as_str().unwrap()).unwrap(),
            value: e["amount"].as_str().unwrap().parse().unwrap(),
            valid_after: now - 30,
            valid_before: now + 60,
            nonce,
        };
        tweak(&mut auth);
        let token = Address::parse_any(e["asset"].as_str().unwrap()).unwrap();
        let td = auth.typed_data("USDC", "2", 84532, &token);
        let sig = sign_digest(key, &super::super::eip712::digest(&td).unwrap());
        // What is signed and what is sent may differ only where a test says.
        x402::payload(
            offer,
            e,
            json!({"signature": evm::hex0x(&sig), "authorization": auth.to_json()}),
        )
    }

    /// The mock takes a payment that holds, and answers each one that does
    /// not with x402's code for it: another payee, another amount, a window
    /// that is over, a signature by someone else, an entry it never offered,
    /// a nonce taken twice.
    #[test]
    fn the_mock_holds_a_payment_to_what_it_offered() {
        let st = State {
            knobs: default_knobs(),
            origin: "http://127.0.0.1:1".into(),
            nonces: BTreeSet::new(),
            record: Vec::new(),
        };
        let offer = required(&st, "/data");
        assert_eq!(offer["resource"]["url"], "http://127.0.0.1:1/data");
        let key = key_from_bytes(&[0x42; 32]).unwrap();
        let none = |_: &mut Authorization| {};
        let (from, _) = check(&pay(&offer, &key, [1; 32], &none), &offer).unwrap();
        assert_eq!(from, address_of_key(&key));
        let code = |p: Value| check(&p, &offer).unwrap_err();
        assert_eq!(
            code(pay(&offer, &key, [2; 32], &|a| a.to = Address([7; 20]))),
            "invalid_exact_evm_payload_recipient_mismatch"
        );
        assert_eq!(
            code(pay(&offer, &key, [3; 32], &|a| a.value += 1)),
            "invalid_exact_evm_payload_authorization_value_mismatch"
        );
        assert_eq!(
            code(pay(&offer, &key, [4; 32], &|a| a.valid_before = 1)),
            "invalid_exact_evm_payload_authorization_valid_before"
        );
        let mut forged = pay(&offer, &key, [5; 32], &none);
        forged["payload"]["authorization"]["from"] =
            json!("0x857b06519E91e3A54538791bDbb0E22373e36b66");
        assert_eq!(code(forged), "invalid_exact_evm_payload_signature");
        let mut other = pay(&offer, &key, [6; 32], &none);
        other["accepted"]["amount"] = json!("1");
        assert_eq!(code(other), "invalid_payment_requirements");
        let mut v1 = pay(&offer, &key, [7; 32], &none);
        v1["x402Version"] = json!(1);
        assert_eq!(code(v1), "invalid_x402_version");
    }
}
