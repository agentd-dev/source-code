// SPDX-License-Identifier: AGPL-3.0-only
//! EIP-712 typed structured data: the digest a wallet signs (RFC 0049 D5,
//! D8). It is computed from the typed data in the JSON form
//! `eth_signTypedData_v4` takes — `{types, primaryType, domain, message}`
//! — so agentd, which builds an EIP-3009 authorization as typed data, and
//! the external signer, which is sent the typed data and reads it before it
//! signs, compute it by one rule.
//!
//! - `encodeType` is the struct's `Name(type name,…)`, followed by every
//!   struct type it references, directly or not, sorted by name.
//! - `hashStruct(s)` is the Keccak-256 of `typeHash ‖ encodeData(s)`, each
//!   member in declaration order as one 32-byte word: `address`, `bool`,
//!   `uintN` and `bytesN` as their ABI encoding; `string` and `bytes` as the
//!   Keccak-256 of their contents; a struct as its own `hashStruct`.
//! - The digest is `keccak256(0x19 ‖ 0x01 ‖ domainSeparator ‖
//!   hashStruct(message))`, the domain separator the `hashStruct` of the
//!   domain under its `EIP712Domain` type.
//!
//! Strict where a signature is at stake: a member missing from a value, or a
//! member the type does not declare, is an error rather than a zero or a
//! silent drop — nothing is signed that the typed data does not say. Arrays
//! and signed integers are not taken: no x402 scheme agentd signs has them.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::evm::{Address, keccak256, unhex0x};

/// A struct type's members, in declaration order: (name, type).
type Members = Vec<(String, String)>;

/// The `types` of typed data, read.
pub struct Types(BTreeMap<String, Members>);

impl Types {
    /// Read `types`: `{Name: [{name, type}, …]}`.
    pub fn parse(v: &Value) -> Result<Types, String> {
        let o = v.as_object().ok_or("types is not an object")?;
        let mut out = BTreeMap::new();
        for (name, members) in o {
            if !identifier(name) {
                return Err(format!("{name:?} is not a type name"));
            }
            let list = members
                .as_array()
                .ok_or_else(|| format!("types.{name} is not a list"))?;
            let mut seen = BTreeSet::new();
            let mut m = Vec::with_capacity(list.len());
            for f in list {
                let (Some(n), Some(t)) = (f["name"].as_str(), f["type"].as_str()) else {
                    return Err(format!("types.{name}: each member is {{name, type}}"));
                };
                if !identifier(n) || !seen.insert(n.to_string()) {
                    return Err(format!(
                        "types.{name}: member {n:?} is not a name, or twice"
                    ));
                }
                m.push((n.to_string(), t.to_string()));
            }
            out.insert(name.clone(), m);
        }
        Ok(Types(out))
    }

    /// `encodeType(name)`.
    pub fn encode_type(&self, name: &str) -> Result<String, String> {
        let mut deps = BTreeSet::new();
        self.dependencies(name, &mut deps)?;
        deps.remove(name);
        let mut out = self.one(name)?;
        for d in deps {
            out.push_str(&self.one(&d)?);
        }
        Ok(out)
    }

    /// `typeHash(name)`.
    pub fn type_hash(&self, name: &str) -> Result<[u8; 32], String> {
        Ok(keccak256(self.encode_type(name)?.as_bytes()))
    }

    /// `hashStruct(name, value)`.
    pub fn hash_struct(&self, name: &str, value: &Value) -> Result<[u8; 32], String> {
        let members = self
            .0
            .get(name)
            .ok_or_else(|| format!("no type {name:?}"))?;
        let o = value
            .as_object()
            .ok_or_else(|| format!("a {name} value is not an object"))?;
        if let Some(extra) = o.keys().find(|k| !members.iter().any(|(n, _)| n == *k)) {
            return Err(format!(
                "a {name} value has {extra:?}, which {name} does not declare"
            ));
        }
        let mut data = Vec::with_capacity(32 * (members.len() + 1));
        data.extend_from_slice(&self.type_hash(name)?);
        for (field, ty) in members {
            let v = o
                .get(field)
                .ok_or_else(|| format!("a {name} value has no {field:?}"))?;
            data.extend_from_slice(
                &self
                    .encode_value(ty, v)
                    .map_err(|e| format!("{name}.{field}: {e}"))?,
            );
        }
        Ok(keccak256(&data))
    }

    fn one(&self, name: &str) -> Result<String, String> {
        let members = self
            .0
            .get(name)
            .ok_or_else(|| format!("no type {name:?}"))?;
        let inner: Vec<String> = members.iter().map(|(n, t)| format!("{t} {n}")).collect();
        Ok(format!("{name}({})", inner.join(",")))
    }

    fn dependencies(&self, name: &str, out: &mut BTreeSet<String>) -> Result<(), String> {
        if out.contains(name) {
            return Ok(());
        }
        let members = self
            .0
            .get(name)
            .ok_or_else(|| format!("no type {name:?}"))?;
        out.insert(name.to_string());
        for (_, t) in members {
            if self.0.contains_key(t) {
                self.dependencies(t, out)?;
            } else if !atomic(t) {
                return Err(format!(
                    "{name} has a member of type {t:?}, which is neither atomic nor declared"
                ));
            }
        }
        Ok(())
    }

    /// One member's 32-byte word.
    fn encode_value(&self, ty: &str, v: &Value) -> Result<[u8; 32], String> {
        if self.0.contains_key(ty) {
            return self.hash_struct(ty, v);
        }
        let mut w = [0u8; 32];
        match ty {
            "address" => {
                let a = v
                    .as_str()
                    .ok_or("an address is a string")
                    .and_then(|s| Address::parse(s).map_err(|_| "not an address"))?;
                w[12..].copy_from_slice(&a.0);
            }
            "bool" => {
                w[31] = u8::from(v.as_bool().ok_or("a bool is true or false")?);
            }
            "string" => {
                w = keccak256(v.as_str().ok_or("a string is a string")?.as_bytes());
            }
            "bytes" => {
                w = keccak256(&unhex0x(v.as_str().ok_or("bytes are 0x-prefixed hex")?)?);
            }
            t if t.starts_with("bytes") => {
                let n: usize = t[5..].parse().map_err(|_| format!("no type {t:?}"))?;
                let b = unhex0x(v.as_str().ok_or("bytesN are 0x-prefixed hex")?)?;
                if b.len() != n {
                    return Err(format!("{} bytes, not {n}", b.len()));
                }
                w[..n].copy_from_slice(&b);
            }
            t if t.starts_with("uint") => {
                let bits: usize = t[4..].parse().map_err(|_| format!("no type {t:?}"))?;
                w = uint_word(v)?;
                let full = 32 - bits / 8;
                if w[..full].iter().any(|b| *b != 0) {
                    return Err(format!("{v} does not fit in {t}"));
                }
            }
            t => return Err(format!("type {t:?} is not taken")),
        }
        Ok(w)
    }
}

/// The digest of typed data `td`: `keccak256(0x1901 ‖ domainSeparator ‖
/// hashStruct(primaryType, message))`.
pub fn digest(td: &Value) -> Result<[u8; 32], String> {
    let types = Types::parse(&td["types"])?;
    if !types.0.contains_key("EIP712Domain") {
        return Err("types declares no EIP712Domain".into());
    }
    let primary = td["primaryType"].as_str().ok_or("no primaryType")?;
    let domain = types.hash_struct("EIP712Domain", &td["domain"])?;
    let message = types.hash_struct(primary, &td["message"])?;
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&domain);
    buf.extend_from_slice(&message);
    Ok(keccak256(&buf))
}

/// `[A-Za-z_$][A-Za-z0-9_$]*`: a Solidity identifier.
fn identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_' || f == '$')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '$')
}

/// Whether `t` is an atomic type this encoder takes.
fn atomic(t: &str) -> bool {
    match t {
        "address" | "bool" | "string" | "bytes" => true,
        _ => {
            let width = |rest: &str, max: usize, step: usize| {
                rest.parse::<usize>()
                    .is_ok_and(|n| n >= step && n <= max && n % step == 0 && !rest.starts_with('0'))
            };
            t.strip_prefix("uint").is_some_and(|r| width(r, 256, 8))
                || t.strip_prefix("bytes").is_some_and(|r| width(r, 32, 1))
        }
    }
}

/// An unsigned integer as a big-endian word: a JSON number, a decimal
/// string, or `0x` hex.
fn uint_word(v: &Value) -> Result<[u8; 32], String> {
    let mut w = [0u8; 32];
    match v {
        Value::Number(n) => {
            let n = n.as_u64().ok_or("a number that is not a whole u64")?;
            w[24..].copy_from_slice(&n.to_be_bytes());
        }
        Value::String(s) if s.starts_with("0x") => {
            let b = unhex0x(&if s.len() % 2 == 1 {
                format!("0x0{}", &s[2..])
            } else {
                s.clone()
            })?;
            if b.len() > 32 {
                return Err("more than 256 bits".into());
            }
            w[32 - b.len()..].copy_from_slice(&b);
        }
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            for d in s.bytes() {
                // w = w * 10 + d, in 256 bits.
                let mut carry = u16::from(d - b'0');
                for byte in w.iter_mut().rev() {
                    let x = u16::from(*byte) * 10 + carry;
                    *byte = (x & 0xff) as u8;
                    carry = x >> 8;
                }
                if carry != 0 {
                    return Err("more than 256 bits".into());
                }
            }
        }
        _ => return Err(format!("{v} is not an unsigned integer")),
    }
    Ok(w)
}

/// The typed data of an EIP-712 message whose types are `types` (each a
/// list of `(name, type)`): the shape `eth_signTypedData_v4` takes.
pub fn typed_data(
    types: &[(&str, &[(&str, &str)])],
    primary: &str,
    domain: Value,
    message: Value,
) -> Value {
    let mut t = Map::new();
    for (name, members) in types {
        t.insert(
            name.to_string(),
            Value::Array(
                members
                    .iter()
                    .map(|(n, ty)| serde_json::json!({"name": n, "type": ty}))
                    .collect(),
            ),
        );
    }
    serde_json::json!({"types": t, "primaryType": primary, "domain": domain, "message": message})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payments::evm::{address_of_key, hex0x, key_from_bytes, recover, sign_digest};
    use serde_json::json;

    /// The `Mail` example of EIP-712 itself, as its reference test
    /// (`assets/eip-712/Example.js`) has it.
    fn mail() -> Value {
        typed_data(
            &[
                (
                    "EIP712Domain",
                    &[
                        ("name", "string"),
                        ("version", "string"),
                        ("chainId", "uint256"),
                        ("verifyingContract", "address"),
                    ],
                ),
                ("Person", &[("name", "string"), ("wallet", "address")]),
                (
                    "Mail",
                    &[("from", "Person"), ("to", "Person"), ("contents", "string")],
                ),
            ],
            "Mail",
            json!({"name": "Ether Mail", "version": "1", "chainId": 1,
                   "verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"}),
            json!({"from": {"name": "Cow", "wallet": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"},
                   "to": {"name": "Bob", "wallet": "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB"},
                   "contents": "Hello, Bob!"}),
        )
    }

    /// Every value EIP-712's own example states: `encodeType`, the type
    /// hash, `hashStruct` of the message, the domain separator and the
    /// digest — and the signature its key, `keccak256("cow")`, makes of it,
    /// byte for byte (RFC 6979), recovering to the `from` wallet.
    #[test]
    fn the_eip712_mail_example_hashes_and_signs_as_the_specification_says() {
        let td = mail();
        let types = Types::parse(&td["types"]).unwrap();
        assert_eq!(
            types.encode_type("Mail").unwrap(),
            "Mail(Person from,Person to,string contents)Person(string name,address wallet)"
        );
        assert_eq!(
            hex0x(&types.type_hash("Mail").unwrap()),
            "0xa0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2"
        );
        assert_eq!(
            hex0x(&types.hash_struct("Mail", &td["message"]).unwrap()),
            "0xc52c0ee5d84264471806290a3f2c4cecfc5490626bf912d01f240d7a274b371e"
        );
        assert_eq!(
            hex0x(&types.hash_struct("EIP712Domain", &td["domain"]).unwrap()),
            "0xf2cee375fa42b42143804025fc449deafd50cc031ca257e0b194a650a912090f"
        );
        let d = digest(&td).unwrap();
        assert_eq!(
            hex0x(&d),
            "0xbe609aee343fb3c4b28e1df9e632fca64fcfaede20f02e86244efddf30957bd2"
        );
        let cow = key_from_bytes(&keccak256(b"cow")).unwrap();
        assert_eq!(
            address_of_key(&cow).checksummed(),
            "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"
        );
        let sig = sign_digest(&cow, &d);
        assert_eq!(sig[64], 28, "v");
        assert_eq!(
            hex0x(&sig[..32]),
            "0x4355c47d63924e8a72e509b65029052eb6c299d53a04e167c5775fd466751c9d"
        );
        assert_eq!(
            hex0x(&sig[32..64]),
            "0x07299936d304c153f6443dfa05f40ff007d72911b6f72307f996231605b91562"
        );
        assert_eq!(recover(&d, &sig).unwrap(), address_of_key(&cow));
    }

    /// Nothing is signed that the typed data does not say: a missing member,
    /// one the type does not declare, an unknown type, a value too wide for
    /// its type, and a domain with no type are refused.
    #[test]
    fn typed_data_that_does_not_say_everything_is_refused() {
        let with = |f: &dyn Fn(&mut Value)| {
            let mut td = mail();
            f(&mut td);
            digest(&td)
        };
        let refused = [
            with(&|t| {
                t["message"]["from"]
                    .as_object_mut()
                    .unwrap()
                    .remove("wallet");
            }),
            with(&|t| t["message"]["cc"] = json!("eve")),
            with(&|t| t["types"]["Mail"][2]["type"] = json!("Contents")),
            with(&|t| t["domain"]["chainId"] = json!(format!("0x{}", "f".repeat(65)))),
            with(&|t| {
                t["types"].as_object_mut().unwrap().remove("EIP712Domain");
            }),
            with(&|t| t["message"]["to"]["wallet"] = json!("0xbob")),
            with(&|t| t["primaryType"] = json!("Letter")),
        ];
        for (i, r) in refused.iter().enumerate() {
            assert!(r.is_err(), "case {i} must be refused");
        }
        // A uint written as a decimal string, a number and hex is one value.
        let a = with(&|t| t["domain"]["chainId"] = json!("1")).unwrap();
        let b = with(&|t| t["domain"]["chainId"] = json!("0x1")).unwrap();
        assert_eq!(a, digest(&mail()).unwrap());
        assert_eq!(a, b);
        let narrow = typed_data(
            &[
                ("EIP712Domain", &[("name", "string")]),
                ("T", &[("v", "uint8")]),
            ],
            "T",
            json!({"name": "n"}),
            json!({"v": "256"}),
        );
        assert!(digest(&narrow).unwrap_err().contains("does not fit"));
    }
}
