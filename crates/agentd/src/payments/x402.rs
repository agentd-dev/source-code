// SPDX-License-Identifier: AGPL-3.0-only
//! The x402 v2 wire as a paying client reads and writes it (RFC 0049 §2):
//! the `PaymentRequired` a 402 carries, the `PaymentPayload` agentd sends
//! back, and the `SettlementResponse`, each a base64 JSON header on HTTP
//! [X-HTTP]; and the EIP-3009 authorization of the `exact` scheme on EVM
//! as EIP-712 typed data [EXACT-EVM].
//!
//! Nothing here decides whether to pay ([`super::policy`]) or signs
//! ([`super::wallet`]): this is the shape of the messages, read strictly —
//! a `PaymentRequired` that is not version 2, has no `accepts`, or a
//! `resource` with no `url` is no payment request.

use serde_json::{Value, json};

use super::evm::{Address, hex0x};

/// The header a 402 carries its `PaymentRequired` in.
pub const PAYMENT_REQUIRED: &str = "PAYMENT-REQUIRED";
/// The header agentd sends its `PaymentPayload` in.
pub const PAYMENT_SIGNATURE: &str = "PAYMENT-SIGNATURE";
/// The header a server answers its `SettlementResponse` in.
pub const PAYMENT_RESPONSE: &str = "PAYMENT-RESPONSE";
/// The protocol version agentd speaks.
pub const VERSION: u64 = 2;
/// The largest header agentd decodes: 64 KiB.
pub const MAX_HEADER: usize = 64 * 1024;

/// The EIP-3009 authorization's EIP-712 types.
pub const TRANSFER_WITH_AUTHORIZATION: &[(&str, &str)] = &[
    ("from", "address"),
    ("to", "address"),
    ("value", "uint256"),
    ("validAfter", "uint256"),
    ("validBefore", "uint256"),
    ("nonce", "bytes32"),
];
/// The token's EIP-712 domain types.
pub const EIP712_DOMAIN: &[(&str, &str)] = &[
    ("name", "string"),
    ("version", "string"),
    ("chainId", "uint256"),
    ("verifyingContract", "address"),
];

/// A header's base64 JSON object: standard or URL-safe base64, padded or
/// not.
pub fn decode_header(value: &str) -> Result<Value, String> {
    let t = value.trim();
    if t.len() > MAX_HEADER {
        return Err(format!(
            "a header of {} bytes (at most {MAX_HEADER})",
            t.len()
        ));
    }
    let bytes = crate::b64::url_decode(&t.replace('+', "-").replace('/', "_").replace('=', ""))
        .map_err(|_| "not base64".to_string())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| "not JSON".to_string())?;
    if !v.is_object() {
        return Err("not a JSON object".into());
    }
    Ok(v)
}

/// `v` as a header value: standard base64, padded, as [X-HTTP] writes it.
pub fn encode_header(v: &Value) -> String {
    crate::b64::std_pad(v.to_string().as_bytes())
}

/// A `PaymentRequired` [X §5.1].
#[derive(Debug, Clone)]
pub struct Required {
    /// The object as the server sent it.
    pub raw: Value,
    /// `resource.url`.
    pub url: String,
    /// `resource.description`, the server's words.
    pub description: Option<String>,
    /// `accepts`, each as sent.
    pub accepts: Vec<Value>,
}

impl Required {
    /// Read a `PaymentRequired` object.
    pub fn parse(v: Value) -> Result<Required, String> {
        if v["x402Version"].as_u64() != Some(VERSION) {
            return Err(format!(
                "x402Version {} (agentd speaks {VERSION})",
                v["x402Version"]
            ));
        }
        let url = v["resource"]["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .ok_or("no resource.url")?
            .to_string();
        let accepts = v["accepts"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("no accepts")?
            .clone();
        Ok(Required {
            description: v["resource"]["description"].as_str().map(str::to_string),
            url,
            accepts,
            raw: v,
        })
    }

    /// The header's `PaymentRequired`.
    pub fn from_header(value: &str) -> Result<Required, String> {
        Required::parse(decode_header(value)?)
    }
}

/// One `accepts` entry's fields, read [X §5.1.2]; `extra` as sent.
#[derive(Debug, Clone)]
pub struct Entry {
    pub scheme: String,
    pub network: String,
    /// `amount`, atomic units, as sent.
    pub amount: String,
    pub asset: String,
    pub pay_to: String,
    pub max_timeout: u64,
    pub extra: Value,
}

impl Entry {
    pub fn parse(v: &Value) -> Result<Entry, String> {
        let s = |k: &str| {
            v[k].as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("no {k}"))
        };
        Ok(Entry {
            scheme: s("scheme")?,
            network: s("network")?,
            amount: s("amount")?,
            asset: s("asset")?,
            pay_to: s("payTo")?,
            max_timeout: v["maxTimeoutSeconds"]
                .as_u64()
                .ok_or("no maxTimeoutSeconds")?,
            extra: v.get("extra").cloned().unwrap_or(Value::Null),
        })
    }
}

/// An EIP-3009 `TransferWithAuthorization` [EXACT-EVM §1].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub from: Address,
    pub to: Address,
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub nonce: [u8; 32],
}

impl Authorization {
    /// As the `payload.authorization` of a `PaymentPayload`: every number a
    /// decimal string.
    pub fn to_json(&self) -> Value {
        json!({
            "from": self.from.checksummed(),
            "to": self.to.checksummed(),
            "value": self.value.to_string(),
            "validAfter": self.valid_after.to_string(),
            "validBefore": self.valid_before.to_string(),
            "nonce": hex0x(&self.nonce),
        })
    }

    /// The typed data a wallet signs for it, under the token's domain.
    pub fn typed_data(
        &self,
        name: &str,
        version: &str,
        chain_id: u64,
        contract: &Address,
    ) -> Value {
        super::eip712::typed_data(
            &[
                ("EIP712Domain", EIP712_DOMAIN),
                ("TransferWithAuthorization", TRANSFER_WITH_AUTHORIZATION),
            ],
            "TransferWithAuthorization",
            json!({"name": name, "version": version, "chainId": chain_id,
                   "verifyingContract": contract.checksummed()}),
            self.to_json(),
        )
    }
}

/// What typed data says when it is an EIP-3009 authorization, and only
/// then (RFC 0049 D8): exactly the members `typed_data` writes — `types`
/// of `EIP712Domain` and `TransferWithAuthorization` alone, each member of
/// its type and in its order, the `primaryType`, a domain of exactly
/// `name`, `version`, `chainId` and `verifyingContract`, a message of
/// exactly the six fields. What an external signer reads before it signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub contract: Address,
    pub authorization: Authorization,
}

impl Transfer {
    pub fn read(td: &Value) -> Result<Transfer, String> {
        let only = |v: &Value, keys: &[&str], what: &str| -> Result<(), String> {
            let o = v
                .as_object()
                .ok_or_else(|| format!("{what} is not an object"))?;
            let mut have: Vec<&str> = o.keys().map(String::as_str).collect();
            have.sort_unstable();
            let mut want = keys.to_vec();
            want.sort_unstable();
            if have == want {
                Ok(())
            } else {
                Err(format!("{what} is not exactly {}", keys.join(", ")))
            }
        };
        only(
            td,
            &["types", "primaryType", "domain", "message"],
            "the typed data",
        )?;
        if td["primaryType"] != "TransferWithAuthorization" {
            return Err("the primaryType is not TransferWithAuthorization".into());
        }
        only(
            &td["types"],
            &["EIP712Domain", "TransferWithAuthorization"],
            "types",
        )?;
        let members = |name: &str, want: &[(&str, &str)]| -> Result<(), String> {
            let ok = td["types"][name].as_array().is_some_and(|a| {
                a.len() == want.len()
                    && a.iter().zip(want).all(|(m, (n, t))| {
                        m["name"] == *n
                            && m["type"] == *t
                            && m.as_object().is_some_and(|o| o.len() == 2)
                    })
            });
            if ok {
                Ok(())
            } else {
                Err(format!("types.{name} is not EIP-3009's"))
            }
        };
        members("EIP712Domain", EIP712_DOMAIN)?;
        members("TransferWithAuthorization", TRANSFER_WITH_AUTHORIZATION)?;
        let d = &td["domain"];
        only(
            d,
            &["name", "version", "chainId", "verifyingContract"],
            "the domain",
        )?;
        let m = &td["message"];
        only(
            m,
            &["from", "to", "value", "validAfter", "validBefore", "nonce"],
            "the message",
        )?;
        let address = |v: &Value, what: &str| {
            v.as_str()
                .and_then(|s| Address::parse_any(s).ok())
                .ok_or_else(|| format!("{what} is not an address"))
        };
        let number = |v: &Value, what: &str| -> Result<u128, String> {
            match v {
                Value::Number(n) => n.as_u64().map(u128::from),
                Value::String(s) => crate::config::settings::payments::parse_atomic(s).ok(),
                _ => None,
            }
            .ok_or_else(|| format!("{what} is not a whole number"))
        };
        let nonce = m["nonce"]
            .as_str()
            .and_then(|s| super::evm::unhex0x(s).ok())
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .ok_or("the nonce is not 32 bytes")?;
        let seconds = |v: &Value, what: &str| -> Result<u64, String> {
            u64::try_from(number(v, what)?).map_err(|_| format!("{what} is past 64 bits"))
        };
        Ok(Transfer {
            name: d["name"]
                .as_str()
                .ok_or("the domain's name is not a string")?
                .to_string(),
            version: d["version"]
                .as_str()
                .ok_or("the domain's version is not a string")?
                .to_string(),
            chain_id: seconds(&d["chainId"], "chainId")?,
            contract: address(&d["verifyingContract"], "verifyingContract")?,
            authorization: Authorization {
                from: address(&m["from"], "from")?,
                to: address(&m["to"], "to")?,
                value: number(&m["value"], "value")?,
                valid_after: seconds(&m["validAfter"], "validAfter")?,
                valid_before: seconds(&m["validBefore"], "validBefore")?,
                nonce,
            },
        })
    }
}

/// The `PaymentPayload` [X §5.2]: the server's `resource` and `extensions`
/// as it sent them, the entry chosen as it sent it, and the scheme's
/// `payload` — on EVM the signature and the authorization it signs.
pub fn payload(required: &Value, accepted: &Value, payload: Value) -> Value {
    let mut p = json!({
        "x402Version": VERSION,
        "resource": required["resource"],
        "accepted": accepted,
        "payload": payload,
    });
    if let Some(ext) = required.get("extensions").filter(|e| !e.is_null()) {
        p["extensions"] = ext.clone();
    }
    p
}

/// A `SettlementResponse` [X §5.3].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub success: bool,
    pub transaction: String,
    pub network: String,
    pub payer: Option<String>,
    pub error_reason: Option<String>,
}

impl Settlement {
    pub fn from_header(value: &str) -> Result<Settlement, String> {
        let v = decode_header(value)?;
        Ok(Settlement {
            success: v["success"].as_bool().ok_or("no success")?,
            transaction: v["transaction"].as_str().unwrap_or_default().to_string(),
            network: v["network"].as_str().unwrap_or_default().to_string(),
            payer: v["payer"].as_str().map(str::to_string),
            error_reason: v["errorReason"].as_str().map(str::to_string),
        })
    }

    /// Settlement was broadcast and not confirmed, or a transaction exists
    /// though it says it failed: what landed is unknown [X §9].
    pub fn pending(&self) -> bool {
        self.error_reason.as_deref() == Some("settlement_pending")
            || (!self.success && !self.transaction.is_empty())
    }
}

/// Whether the `resource.url` a server named is the URL agentd asked: the
/// same scheme, host (any case) and port, the same path, and — when the
/// resource names a query — the same query. A server answering for one
/// resource with a payment for another is never paid.
pub fn same_resource(requested: &str, resource: &str) -> bool {
    let (Ok(a), Ok(b)) = (
        crate::net::http::Url::parse(requested),
        crate::net::http::Url::parse(resource),
    ) else {
        return false;
    };
    let split = |p: &str| -> (String, Option<String>) {
        match p.split_once('?') {
            Some((path, q)) => (path.to_string(), Some(q.to_string())),
            None => (p.to_string(), None),
        }
    };
    let (pa, qa) = split(&a.path);
    let (pb, qb) = split(&b.path);
    a.is_tls() == b.is_tls()
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.port == b.port
        && pa == pb
        && (qb.is_none() || qa == qb)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [X-HTTP]'s own `PAYMENT-REQUIRED` example decodes to the
    /// `PaymentRequired` the page shows, and its entry reads field by field.
    #[test]
    fn the_http_transports_example_header_decodes() {
        let header = "eyJ4NDAyVmVyc2lvbiI6MiwiZXJyb3IiOiJQQVlNRU5ULVNJR05BVFVSRSBoZWFkZXIgaXMgcmVxdWlyZWQiLCJyZXNvdXJjZSI6eyJ1cmwiOiJodHRwczovL2FwaS5leGFtcGxlLmNvbS9wcmVtaXVtLWRhdGEiLCJkZXNjcmlwdGlvbiI6IkFjY2VzcyB0byBwcmVtaXVtIG1hcmtldCBkYXRhIiwibWltZVR5cGUiOiJhcHBsaWNhdGlvbi9qc29uIn0sImFjY2VwdHMiOlt7InNjaGVtZSI6ImV4YWN0IiwibmV0d29yayI6ImVpcDE1NTo4NDUzMiIsImFtb3VudCI6IjEwMDAwIiwiYXNzZXQiOiIweDAzNkNiRDUzODQyYzU0MjY2MzRlNzkyOTU0MWVDMjMxOGYzZENGN2UiLCJwYXlUbyI6IjB4MjA5NjkzQmM2YWZjMEM1MzI4YkEzNkZhRjAzQzUxNEVGMzEyMjg3QyIsIm1heFRpbWVvdXRTZWNvbmRzIjo2MCwiZXh0cmEiOnsibmFtZSI6IlVTREMiLCJ2ZXJzaW9uIjoiMiJ9fV19";
        let r = Required::from_header(header).unwrap();
        assert_eq!(r.url, "https://api.example.com/premium-data");
        assert_eq!(
            r.description.as_deref(),
            Some("Access to premium market data")
        );
        let e = Entry::parse(&r.accepts[0]).unwrap();
        assert_eq!(e.scheme, "exact");
        assert_eq!(e.network, "eip155:84532");
        assert_eq!(e.amount, "10000");
        assert_eq!(e.asset, "0x036CbD53842c5426634e7929541eC2318f3dCF7e");
        assert_eq!(e.pay_to, "0x209693Bc6afc0C5328bA36FaF03C514EF312287C");
        assert_eq!(e.max_timeout, 60);
        assert_eq!(e.extra, json!({"name": "USDC", "version": "2"}));
        // Round trip.
        assert_eq!(decode_header(&encode_header(&r.raw)).unwrap(), r.raw);
    }

    /// A version other than 2, no accepts, no resource URL, a header that
    /// is not base64 JSON or is too large: no payment request.
    #[test]
    fn what_is_no_payment_request() {
        let ok = json!({"x402Version": 2, "resource": {"url": "https://a/b"},
                        "accepts": [{"scheme": "exact"}]});
        assert!(Required::parse(ok.clone()).is_ok());
        for bad in [
            json!({"x402Version": 1, "resource": {"url": "https://a/b"}, "accepts": [{}]}),
            json!({"x402Version": 2, "resource": {"url": "https://a/b"}, "accepts": []}),
            json!({"x402Version": 2, "resource": {}, "accepts": [{}]}),
        ] {
            assert!(Required::parse(bad).is_err());
        }
        assert!(decode_header("not base64!").is_err());
        assert!(decode_header(&crate::b64::std_pad(b"[1,2]")).is_err());
        assert!(decode_header(&"A".repeat(MAX_HEADER + 4)).is_err());
    }

    /// [X-HTTP]'s settlement examples, and what an unknown outcome looks
    /// like: `settlement_pending`, or a failure that names a transaction.
    #[test]
    fn settlement_responses_read() {
        let ok = Settlement::from_header("eyJzdWNjZXNzIjp0cnVlLCJ0cmFuc2FjdGlvbiI6IjB4MTIzNDU2Nzg5MGFiY2RlZjEyMzQ1Njc4OTBhYmNkZWYxMjM0NTY3ODkwYWJjZGVmMTIzNDU2Nzg5MGFiY2RlZiIsIm5ldHdvcmsiOiJlaXAxNTU6ODQ1MzIiLCJwYXllciI6IjB4ODU3YjA2NTE5RTkxZTNBNTQ1Mzg3OTFiRGJiMEUyMjM3M2UzNmI2NiJ9").unwrap();
        assert!(ok.success && !ok.pending());
        assert_eq!(ok.network, "eip155:84532");
        assert_eq!(
            ok.payer.as_deref(),
            Some("0x857b06519E91e3A54538791bDbb0E22373e36b66")
        );
        let failed = Settlement::from_header("eyJzdWNjZXNzIjpmYWxzZSwiZXJyb3JSZWFzb24iOiJpbnN1ZmZpY2llbnRfZnVuZHMiLCJ0cmFuc2FjdGlvbiI6IiIsIm5ldHdvcmsiOiJlaXAxNTU6ODQ1MzIiLCJwYXllciI6IjB4ODU3YjA2NTE5RTkxZTNBNTQ1Mzg3OTFiRGJiMEUyMjM3M2UzNmI2NiJ9").unwrap();
        assert!(!failed.success && !failed.pending());
        assert_eq!(failed.error_reason.as_deref(), Some("insufficient_funds"));
        let pending = Settlement::from_header(&encode_header(&json!({
            "success": false, "errorReason": "settlement_pending",
            "transaction": "0xab", "network": "eip155:8453"})))
        .unwrap();
        assert!(pending.pending());
        let broadcast = Settlement::from_header(&encode_header(&json!({
            "success": false, "transaction": "0xab", "network": "eip155:8453"})))
        .unwrap();
        assert!(broadcast.pending());
    }

    /// The authorization's typed data hashes as an EIP-3009 token computes
    /// it on chain: EIP-3009's `TRANSFER_WITH_AUTHORIZATION_TYPEHASH`, the
    /// well-known `EIP712Domain` type hash, and the digest built word by
    /// word the way the contract's `abi.encode` does — independently of the
    /// typed-data encoder.
    #[test]
    fn eip3009_typed_data_hashes_as_the_token_contract_does() {
        use super::super::eip712::{Types, digest};
        use super::super::evm::keccak256;
        let auth = Authorization {
            from: Address::parse("0x857b06519E91e3A54538791bDbb0E22373e36b66").unwrap(),
            to: Address::parse("0x209693Bc6afc0C5328bA36FaF03C514EF312287C").unwrap(),
            value: 10_000,
            valid_after: 1_740_672_089,
            valid_before: 1_740_672_154,
            nonce: <[u8; 32]>::try_from(
                super::super::evm::unhex0x(
                    "0xf3746613c2d920b5fdabc0856f2aeb2d4f88ee6037b8cc5d04a71a4462f13480",
                )
                .unwrap()
                .as_slice(),
            )
            .unwrap(),
        };
        let usdc = Address::parse("0x036CbD53842c5426634e7929541eC2318f3dCF7e").unwrap();
        let td = auth.typed_data("USDC", "2", 84532, &usdc);
        let types = Types::parse(&td["types"]).unwrap();
        assert_eq!(
            hex0x(&types.type_hash("TransferWithAuthorization").unwrap()),
            "0x7c7c6cdb67a18743f49ec6fa9b35f50d52ed05cbed4cc592e13b44501c1a2267"
        );
        assert_eq!(
            hex0x(&types.type_hash("EIP712Domain").unwrap()),
            "0x8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f"
        );
        let word_u = |n: u128| {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&n.to_be_bytes());
            w
        };
        let word_a = |a: &Address| {
            let mut w = [0u8; 32];
            w[12..].copy_from_slice(&a.0);
            w
        };
        let mut dom = Vec::new();
        dom.extend(types.type_hash("EIP712Domain").unwrap());
        dom.extend(keccak256(b"USDC"));
        dom.extend(keccak256(b"2"));
        dom.extend(word_u(84532));
        dom.extend(word_a(&usdc));
        let mut msg = Vec::new();
        msg.extend(types.type_hash("TransferWithAuthorization").unwrap());
        msg.extend(word_a(&auth.from));
        msg.extend(word_a(&auth.to));
        msg.extend(word_u(auth.value));
        msg.extend(word_u(u128::from(auth.valid_after)));
        msg.extend(word_u(u128::from(auth.valid_before)));
        msg.extend(auth.nonce);
        let mut buf = vec![0x19, 0x01];
        buf.extend(keccak256(&dom));
        buf.extend(keccak256(&msg));
        assert_eq!(digest(&td).unwrap(), keccak256(&buf));
        // A changed field is another digest.
        let mut other = auth.clone();
        other.value += 1;
        assert_ne!(
            digest(&other.typed_data("USDC", "2", 84532, &usdc)).unwrap(),
            digest(&td).unwrap()
        );
        assert_ne!(
            digest(&auth.typed_data("USDC", "2", 8453, &usdc)).unwrap(),
            digest(&td).unwrap(),
            "the chain id is bound"
        );
    }

    /// [X-HTTP]'s own `PAYMENT-SIGNATURE` example is a real signature: its
    /// authorization, hashed under the token domain its `accepted` entry
    /// names (USDC version 2 on Base Sepolia), recovers to its `from`.
    #[test]
    fn the_http_transports_example_payment_recovers_to_its_payer() {
        let header = "eyJ4NDAyVmVyc2lvbiI6MiwicmVzb3VyY2UiOnsidXJsIjoiaHR0cHM6Ly9hcGkuZXhhbXBsZS5jb20vcHJlbWl1bS1kYXRhIiwiZGVzY3JpcHRpb24iOiJBY2Nlc3MgdG8gcHJlbWl1bSBtYXJrZXQgZGF0YSIsIm1pbWVUeXBlIjoiYXBwbGljYXRpb24vanNvbiJ9LCJhY2NlcHRlZCI6eyJzY2hlbWUiOiJleGFjdCIsIm5ldHdvcmsiOiJlaXAxNTU6ODQ1MzIiLCJhbW91bnQiOiIxMDAwMCIsImFzc2V0IjoiMHgwMzZDYkQ1Mzg0MmM1NDI2NjM0ZTc5Mjk1NDFlQzIzMThmM2RDRjdlIiwicGF5VG8iOiIweDIwOTY5M0JjNmFmYzBDNTMyOGJBMzZGYUYwM0M1MTRFRjMxMjI4N0MiLCJtYXhUaW1lb3V0U2Vjb25kcyI6NjAsImV4dHJhIjp7Im5hbWUiOiJVU0RDIiwidmVyc2lvbiI6IjIifX0sInBheWxvYWQiOnsic2lnbmF0dXJlIjoiMHgyZDZhNzU4OGQ2YWNjYTUwNWNiZjBkOWE0YTIyN2UwYzUyYzZjMzQwMDhjOGU4OTg2YTEyODMyNTk3NjQxNzM2MDhhMmNlNjQ5NjY0MmUzNzdkNmRhOGRiYmY1ODM2ZTliZDE1MDkyZjllY2FiMDVkZWQzZDYyOTNhZjE0OGI1NzFjIiwiYXV0aG9yaXphdGlvbiI6eyJmcm9tIjoiMHg4NTdiMDY1MTlFOTFlM0E1NDUzODc5MWJEYmIwRTIyMzczZTM2YjY2IiwidG8iOiIweDIwOTY5M0JjNmFmYzBDNTMyOGJBMzZGYUYwM0M1MTRFRjMxMjI4N0MiLCJ2YWx1ZSI6IjEwMDAwIiwidmFsaWRBZnRlciI6IjE3NDA2NzIwODkiLCJ2YWxpZEJlZm9yZSI6IjE3NDA2NzIxNTQiLCJub25jZSI6IjB4ZjM3NDY2MTNjMmQ5MjBiNWZkYWJjMDg1NmYyYWViMmQ0Zjg4ZWU2MDM3YjhjYzVkMDRhNzFhNDQ2MmYxMzQ4MCJ9fX0=";
        let p = decode_header(header).unwrap();
        let a = &p["payload"]["authorization"];
        let auth = Authorization {
            from: Address::parse(a["from"].as_str().unwrap()).unwrap(),
            to: Address::parse(a["to"].as_str().unwrap()).unwrap(),
            value: a["value"].as_str().unwrap().parse().unwrap(),
            valid_after: a["validAfter"].as_str().unwrap().parse().unwrap(),
            valid_before: a["validBefore"].as_str().unwrap().parse().unwrap(),
            nonce: <[u8; 32]>::try_from(
                super::super::evm::unhex0x(a["nonce"].as_str().unwrap())
                    .unwrap()
                    .as_slice(),
            )
            .unwrap(),
        };
        let usdc = Address::parse(p["accepted"]["asset"].as_str().unwrap()).unwrap();
        let td = auth.typed_data("USDC", "2", 84532, &usdc);
        let sig = super::super::evm::unhex0x(p["payload"]["signature"].as_str().unwrap()).unwrap();
        let d = super::super::eip712::digest(&td).unwrap();
        assert_eq!(super::super::evm::recover(&d, &sig).unwrap(), auth.from);
        assert_eq!(auth.to_json(), *a, "the authorization writes back as sent");
    }

    /// The typed data agentd builds reads back as the transfer it is; any
    /// other shape — another primary type, a member more or less, another
    /// type for one, an extra domain field — is no EIP-3009 authorization.
    #[test]
    fn only_eip3009s_shape_reads_as_a_transfer() {
        let auth = Authorization {
            from: Address::parse("0x857b06519E91e3A54538791bDbb0E22373e36b66").unwrap(),
            to: Address::parse("0x209693Bc6afc0C5328bA36FaF03C514EF312287C").unwrap(),
            value: 10_000,
            valid_after: 1_740_672_089,
            valid_before: 1_740_672_154,
            nonce: [0xf3; 32],
        };
        let usdc = Address::parse("0x036CbD53842c5426634e7929541eC2318f3dCF7e").unwrap();
        let td = auth.typed_data("USDC", "2", 84532, &usdc);
        let t = Transfer::read(&td).unwrap();
        assert_eq!(t.authorization, auth);
        assert_eq!(
            (t.chain_id, t.contract, t.name.as_str(), t.version.as_str()),
            (84532, usdc, "USDC", "2")
        );
        let with = |f: &dyn Fn(&mut Value)| {
            let mut v = td.clone();
            f(&mut v);
            Transfer::read(&v)
        };
        for (i, r) in [
            with(&|v| v["primaryType"] = json!("Permit")),
            with(&|v| v["message"]["memo"] = json!("x")),
            with(&|v| {
                v["message"].as_object_mut().unwrap().remove("nonce");
            }),
            with(&|v| v["types"]["TransferWithAuthorization"][2]["type"] = json!("uint128")),
            with(&|v| v["types"]["Mail"] = json!([])),
            with(&|v| v["domain"]["salt"] = json!("0x00")),
            with(&|v| v["message"]["nonce"] = json!("0x1234")),
            with(&|v| v["extra"] = json!(1)),
            with(&|v| v["types"]["EIP712Domain"][0]["name"] = json!("title")),
        ]
        .iter()
        .enumerate()
        {
            assert!(r.is_err(), "case {i} must not read as a transfer");
        }
        // The digest is the one `eip712` computes over it.
        assert!(super::super::eip712::digest(&td).is_ok());
    }

    /// The resource a 402 names must be the request's: scheme, host in any
    /// case, port, path, and its query when it names one.
    #[test]
    fn a_payment_for_another_resource_is_not_this_ones() {
        let asked = "https://api.example.com/premium-data?ticker=X";
        for (named, same) in [
            ("https://api.example.com/premium-data", true),
            ("https://API.example.com:443/premium-data", true),
            ("https://api.example.com/premium-data?ticker=X", true),
            ("https://api.example.com/premium-data?ticker=Y", false),
            ("https://api.example.com/other", false),
            ("http://api.example.com/premium-data", false),
            ("https://api.example.com:8443/premium-data", false),
            ("https://evil.example/premium-data", false),
            ("mcp://tool/premium-data", false),
        ] {
            assert_eq!(same_resource(asked, named), same, "{named}");
        }
    }
}
