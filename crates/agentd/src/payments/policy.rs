// SPDX-License-Identifier: AGPL-3.0-only
//! The spend policy (RFC 0049 §4; ADR 0003): which `accepts` entry agentd
//! may pay, or why none.
//!
//! An entry is taken only when every check holds: the `exact` scheme and
//! the `authorization` flow; a network of a family agentd pays on, and what
//! that family asks of `extra` ([`super::scheme::Family::terms`]); a token
//! an allowed asset names on that network; a `payTo` an allowed payee is,
//! for that asset; an amount within the step's `max`, the asset's
//! `per_payment`, and — counting what the ledger says was paid or may have
//! been — its `per_run`, `per_day` and `per_payee`. The first entry, in the
//! server's order, that holds is the one paid; every one passed over is
//! named with each reason, so a refusal says everything at once.

use std::collections::BTreeMap;

use serde_json::Value;

use super::caip::{AssetId, ChainId};
use super::scheme::{Account, Family, Terms, Token};
use super::x402::Entry;
use crate::config::settings::payments::{self as cfg, parse_amount, parse_atomic};

/// What a step permits: its wallet, the assets its `max` names with each
/// one's most, and whether it always asks.
pub use crate::config::settings::payments::StepPayment;

/// An asset the operator allows, read.
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub id: AssetId,
    pub family: Family,
    pub token: Token,
    pub decimals: u8,
    pub per_payment: Option<u128>,
    pub per_run: u128,
    pub per_day: u128,
    pub per_payee: u128,
    pub approve_above: Option<u128>,
}

impl Asset {
    /// `atomic` in whole units, with the asset's name: `"0.25 usdc-base"`.
    pub fn show(&self, atomic: u128) -> String {
        format!(
            "{} {}",
            cfg::format_amount(atomic, self.decimals),
            self.name
        )
    }
}

/// A payee the operator allows, read.
#[derive(Debug, Clone)]
pub struct Payee {
    pub name: String,
    pub address: Account,
    /// The assets it may be paid in; `None`, any.
    pub assets: Option<Vec<String>>,
}

/// `security.payments`, read.
#[derive(Debug, Clone)]
pub struct Policy {
    pub approver: String,
    pub approval_timeout: std::time::Duration,
    pub ask_when_tainted: bool,
    pub max_validity: std::time::Duration,
    pub assets: BTreeMap<String, Asset>,
    pub payees: BTreeMap<String, Payee>,
}

impl Policy {
    /// The configuration, checked at load, read into atomic units and
    /// addresses.
    pub fn from_settings(p: &cfg::Payments) -> Result<Policy, String> {
        let mut assets = BTreeMap::new();
        for (name, a) in &p.assets {
            let at = format!("security.payments.assets.{name}");
            let id = AssetId::parse(&a.asset).map_err(|e| format!("{at}.asset: {e}"))?;
            let family = Family::of(&id.chain)
                .ok_or_else(|| format!("{at}.asset: agentd pays on no chain of {}", id.chain))?;
            let token = family.token(&id).map_err(|e| format!("{at}.asset: {e}"))?;
            let amount = |k: &str, v: &Option<String>| -> Result<Option<u128>, String> {
                v.as_deref()
                    .map(|v| parse_amount(v, a.decimals).map_err(|e| format!("{at}.{k}: {e}")))
                    .transpose()
            };
            let need =
                |k: &str, v: Option<u128>| v.ok_or_else(|| format!("{at}.limits.{k} is not set"));
            assets.insert(
                name.clone(),
                Asset {
                    name: name.clone(),
                    family,
                    token,
                    decimals: a.decimals,
                    per_payment: amount("limits.per_payment", &a.limits.per_payment)?,
                    per_run: need("per_run", amount("limits.per_run", &a.limits.per_run)?)?,
                    per_day: need("per_day", amount("limits.per_day", &a.limits.per_day)?)?,
                    per_payee: need(
                        "per_payee",
                        amount("limits.per_payee", &a.limits.per_payee)?,
                    )?,
                    approve_above: amount("approve_above", &a.approve_above)?,
                    id,
                },
            );
        }
        let mut payees = BTreeMap::new();
        for (name, py) in &p.payees {
            payees.insert(
                name.clone(),
                Payee {
                    name: name.clone(),
                    address: Account::parse(&py.address)
                        .map_err(|e| format!("security.payments.payees.{name}.address: {e}"))?,
                    assets: py.assets.clone(),
                },
            );
        }
        Ok(Policy {
            approver: p.approver().to_string(),
            approval_timeout: p.approval_timeout(),
            ask_when_tainted: p.ask_when_tainted(),
            max_validity: p.max_validity(),
            assets,
            payees,
        })
    }
}

/// What the ledger says was paid, or may have been, of one asset: by the
/// run, in the last 24 hours, and to the payee in them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spent {
    pub run: u128,
    pub day: u128,
    pub payee: u128,
}

/// The entry chosen, and everything a signature and a record need of it.
#[derive(Debug, Clone)]
pub struct Choice {
    /// Its index in `accepts`.
    pub index: usize,
    /// The entry exactly as the server sent it: `accepted` in the payload.
    pub entry: Value,
    /// The allowed asset's name, and its CAIP-19 id.
    pub asset: String,
    pub asset_id: String,
    pub payee: String,
    pub pay_to: Account,
    pub network: String,
    pub amount: u128,
    pub token: Token,
    /// What `extra` says a signature is made under.
    pub terms: Terms,
    pub max_timeout: u64,
    /// Above the asset's `approve_above`.
    pub above_threshold: bool,
}

/// An entry passed over, and every reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    pub index: usize,
    pub reasons: Vec<String>,
}

impl Passed {
    pub fn to_json(&self) -> Value {
        serde_json::json!({"entry": self.index, "reasons": self.reasons})
    }
}

/// The first entry of `accepts` the step and the policy allow, or why each
/// was passed over. `spent` is what the ledger counts for an asset and a
/// payee.
pub fn choose(
    policy: &Policy,
    step: &StepPayment,
    accepts: &[Value],
    spent: &dyn Fn(&str, &Account) -> Spent,
) -> Result<Choice, Vec<Passed>> {
    let mut passed = Vec::new();
    for (index, v) in accepts.iter().enumerate() {
        match one(policy, step, index, v, spent) {
            Ok(c) => return Ok(c),
            Err(reasons) => passed.push(Passed { index, reasons }),
        }
    }
    Err(passed)
}

fn one(
    policy: &Policy,
    step: &StepPayment,
    index: usize,
    v: &Value,
    spent: &dyn Fn(&str, &Account) -> Spent,
) -> Result<Choice, Vec<String>> {
    let e = Entry::parse(v).map_err(|e| vec![format!("malformed: {e}")])?;
    let mut why = Vec::new();
    if e.scheme != "exact" {
        why.push(format!("scheme {:?}: agentd pays only exact", e.scheme));
    }
    match e.extra.get("paymentFlow").and_then(Value::as_str) {
        None | Some("authorization") => {}
        Some(f) => why.push(format!(
            "payment_flow {f:?}: agentd pays only in the authorization flow, which settles after the resource is served"
        )),
    }
    if e.max_timeout == 0 {
        why.push("maxTimeoutSeconds 0".into());
    }
    let amount = match parse_atomic(&e.amount) {
        Ok(0) => {
            why.push("amount 0".into());
            None
        }
        Ok(a) => Some(a),
        Err(err) => {
            why.push(format!("amount: {err}"));
            None
        }
    };
    // The network, its family, and what that family asks of `extra`.
    let network = ChainId::parse(&e.network);
    let family = match &network {
        Ok(n) => {
            let f = Family::of(n);
            if f.is_none() {
                why.push(format!(
                    "network {:?}: agentd pays on no chain of this kind",
                    e.network
                ));
            }
            f
        }
        Err(_) => {
            why.push(format!("network {:?}: not a CAIP-2 chain id", e.network));
            None
        }
    };
    let terms = family.and_then(|f| match f.terms(&e.extra) {
        Ok(t) => Some(t),
        Err(reasons) => {
            why.extend(reasons);
            None
        }
    });
    // The token: an asset the operator names, on that network.
    let asset = match (&network, family) {
        (Ok(n), Some(f)) => {
            let on_network: Vec<&Asset> = policy
                .assets
                .values()
                .filter(|a| a.id.chain == *n)
                .collect();
            if on_network.is_empty() {
                why.push(format!(
                    "network {:?}: no allowed asset is on it",
                    e.network
                ));
                None
            } else {
                match f.entry_token(n, &e.asset) {
                    Ok(t) => {
                        let found = on_network.into_iter().find(|a| a.token == t);
                        if found.is_none() {
                            why.push(format!(
                                "asset {:?}: not an allowed asset on {}",
                                e.asset, e.network
                            ));
                        }
                        found
                    }
                    Err(err) => {
                        why.push(format!("asset: {err}"));
                        None
                    }
                }
            }
        }
        _ => None,
    };
    // The step's own bound: an asset its `max` names.
    let step_max = asset.and_then(|a| {
        let m = step.max.iter().find(|(n, _)| n == &a.name).map(|(_, m)| *m);
        if m.is_none() {
            why.push(format!("max: the step does not pay in {}", a.name));
        }
        m
    });
    // The payee.
    let pay_to = family.map(|f| f.entry_account(&e.pay_to));
    let payee = match &pay_to {
        Some(Ok(addr)) => {
            let p = policy.payees.values().find(|p| p.address == *addr);
            match (p, asset) {
                (None, _) => {
                    why.push(format!("payee {addr}: not an allowed payee"));
                    None
                }
                (Some(p), Some(a))
                    if p.assets
                        .as_ref()
                        .is_some_and(|l| !l.iter().any(|x| x == &a.name)) =>
                {
                    why.push(format!("payee {}: not to be paid in {}", p.name, a.name));
                    None
                }
                (Some(p), _) => Some(p),
            }
        }
        Some(Err(err)) => {
            why.push(format!("payee: {err}"));
            None
        }
        None => None,
    };
    if let (Some(a), Some(amount)) = (asset, amount) {
        if let Some(m) = step_max
            && amount > m
        {
            why.push(format!(
                "max: {} is more than the step's {}",
                a.show(amount),
                a.show(m)
            ));
        }
        if let Some(cap) = a.per_payment
            && amount > cap
        {
            why.push(format!(
                "per_payment: {} is more than {}",
                a.show(amount),
                a.show(cap)
            ));
        }
        if let Some(p) = payee {
            let s = spent(&a.name, &p.address);
            let over = |used: u128, cap: u128| used.checked_add(amount).is_none_or(|t| t > cap);
            if over(s.run, a.per_run) {
                why.push(format!(
                    "per_run: {} with {} this run already is more than {}",
                    a.show(amount),
                    a.show(s.run),
                    a.show(a.per_run)
                ));
            }
            if over(s.day, a.per_day) {
                why.push(format!(
                    "per_day: {} with {} in the last 24 hours is more than {}",
                    a.show(amount),
                    a.show(s.day),
                    a.show(a.per_day)
                ));
            }
            if over(s.payee, a.per_payee) {
                why.push(format!(
                    "per_payee: {} with {} to {} in the last 24 hours is more than {}",
                    a.show(amount),
                    a.show(s.payee),
                    p.name,
                    a.show(a.per_payee)
                ));
            }
        }
    }
    if !why.is_empty() {
        return Err(why);
    }
    let (Some(a), Some(p), Some(amount), Some(terms)) = (asset, payee, amount, terms) else {
        return Err(vec!["malformed".into()]);
    };
    Ok(Choice {
        index,
        entry: v.clone(),
        asset: a.name.clone(),
        asset_id: a.id.to_string(),
        payee: p.name.clone(),
        pay_to: p.address,
        network: e.network,
        amount,
        token: a.token,
        terms,
        max_timeout: e.max_timeout,
        above_threshold: a.approve_above.is_some_and(|t| amount > t),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    pub const PAYEE: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";

    /// A policy of one asset (USDC on Base: 6 decimals) and one payee.
    pub fn policy() -> Policy {
        let p: cfg::Payments = serde_json::from_value(json!({
            "approver": "operator",
            "seal_key": "{{secret-file:/run/k}}",
            "wallets": {"ops": {"key_file": "/var/lib/agentd/ops.key"}},
            "assets": {"usdc-base": {
                "asset": format!("eip155:8453/erc20:{USDC}"), "decimals": 6,
                "limits": {"per_payment": "0.10", "per_run": "0.25", "per_day": "1.00", "per_payee": "0.50"},
                "approve_above": "0.05"}},
            "payees": {"weather": {"address": PAYEE}}
        }))
        .unwrap();
        Policy::from_settings(&p).unwrap()
    }

    /// An `accepts` entry as a server writes one, at `amount` atomic units.
    pub fn entry(amount: &str) -> Value {
        json!({"scheme": "exact", "network": "eip155:8453", "amount": amount,
               "asset": USDC, "payTo": PAYEE, "maxTimeoutSeconds": 60,
               "extra": {"name": "USD Coin", "version": "2"}})
    }

    fn step(max: &str) -> StepPayment {
        StepPayment {
            wallet: "ops".into(),
            max: vec![("usdc-base".into(), parse_amount(max, 6).unwrap())],
            approve: false,
        }
    }

    fn none(_: &str, _: &Account) -> Spent {
        Spent::default()
    }

    /// An entry within everything is chosen, with what a signature needs;
    /// above `approve_above` it says so.
    #[test]
    fn an_entry_within_the_policy_is_chosen() {
        let c = choose(&policy(), &step("0.10"), &[entry("40000")], &none).unwrap();
        assert_eq!(c.amount, 40_000);
        assert!(matches!(c.token, Token::Evm { chain_id: 8453, .. }));
        assert_eq!(c.payee, "weather");
        assert_eq!(
            c.terms,
            Terms::Evm {
                name: "USD Coin".into(),
                version: "2".into()
            }
        );
        assert!(!c.above_threshold);
        let c = choose(&policy(), &step("0.10"), &[entry("60000")], &none).unwrap();
        assert!(c.above_threshold, "0.06 is above 0.05");
    }

    /// The first entry the policy allows wins, whatever the server's order;
    /// those passed over are named.
    #[test]
    fn the_first_allowed_entry_wins() {
        let mut other = entry("1000");
        other["scheme"] = json!("upto");
        let r = choose(&policy(), &step("0.10"), &[other, entry("2000")], &none).unwrap();
        assert_eq!(r.index, 1);
    }

    /// Every reason an entry is passed over, each named — unsent.
    #[test]
    fn every_reason_is_named() {
        let p = policy();
        let refused = |e: Value, s: &StepPayment, spent: &dyn Fn(&str, &Account) -> Spent| {
            let r = choose(&p, s, &[e], spent).unwrap_err();
            r[0].reasons.join("; ")
        };
        let with = |f: &dyn Fn(&mut Value)| {
            let mut e = entry("1000");
            f(&mut e);
            e
        };
        let s = step("0.10");
        for (e, want) in [
            (with(&|e| e["scheme"] = json!("upto")), "scheme"),
            (
                with(&|e| e["extra"]["assetTransferMethod"] = json!("permit2")),
                "transfer_method",
            ),
            (
                with(&|e| e["extra"]["paymentFlow"] = json!("upfront")),
                "payment_flow",
            ),
            (
                with(&|e| {
                    e["extra"].as_object_mut().unwrap().remove("version");
                }),
                "extra",
            ),
            (with(&|e| e["network"] = json!("eip155:1")), "network"),
            (
                with(&|e| e["network"] = json!("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp")),
                "no chain of this kind",
            ),
            (with(&|e| e["network"] = json!("eip155")), "not a CAIP-2"),
            (
                with(&|e| e["asset"] = json!("0x036CbD53842c5426634e7929541eC2318f3dCF7e")),
                "asset",
            ),
            (
                with(&|e| e["payTo"] = json!("0x857b06519E91e3A54538791bDbb0E22373e36b66")),
                "payee",
            ),
            (with(&|e| e["amount"] = json!("0")), "amount 0"),
            (with(&|e| e["amount"] = json!("1.5")), "amount"),
            (
                with(&|e| e["maxTimeoutSeconds"] = json!(0)),
                "maxTimeoutSeconds",
            ),
            (entry("100001"), "per_payment"),
        ] {
            let why = refused(e, &s, &none);
            assert!(why.contains(want), "{want}: {why}");
        }
        assert!(refused(entry("60000"), &step("0.05"), &none).contains("max:"));
        let other_asset = StepPayment {
            max: vec![("usdc-other".into(), 1)],
            ..step("0.10")
        };
        assert!(refused(entry("1000"), &other_asset, &none).contains("does not pay in usdc-base"));
        // The caps count what the ledger says.
        let run = |_: &str, _: &Account| Spent {
            run: 200_000,
            ..Spent::default()
        };
        assert!(refused(entry("60000"), &s, &run).contains("per_run"));
        let day = |_: &str, _: &Account| Spent {
            day: 950_000,
            ..Spent::default()
        };
        assert!(refused(entry("60000"), &s, &day).contains("per_day"));
        let payee = |_: &str, _: &Account| Spent {
            payee: 450_000,
            ..Spent::default()
        };
        assert!(refused(entry("60000"), &s, &payee).contains("per_payee"));
        // Exactly at a cap is within it.
        let at = |_: &str, _: &Account| Spent {
            run: 150_000,
            ..Spent::default()
        };
        assert!(choose(&p, &s, &[entry("100000")], &at).is_ok());
        // A payee held to other assets.
        let mut held = policy();
        held.payees.get_mut("weather").unwrap().assets = Some(vec!["eurc".into()]);
        assert!(
            choose(&held, &s, &[entry("1000")], &none).unwrap_err()[0].reasons[0]
                .contains("not to be paid in")
        );
    }
}
