// SPDX-License-Identifier: AGPL-3.0-only
//! Paying for things (RFC 0049; ADR 0003): `security.payments` — the spend
//! policy, the wallets and who holds their keys, the one person who approves
//! a payment above a threshold — and the `payment` field that permits a step
//! of a [`PAYING_KINDS`] kind to pay within it.
//!
//! Parsed whatever the build; without the `x402` feature `security.payments`
//! and every paying step are refused by name. Operator-only (`security` is,
//! RFC 0042) and restart-only: the caps, the allow-lists and the keys are
//! what a payment is judged by.
//!
//! Amounts are decimals in an asset's whole units (`"0.25"`), read exactly
//! with the asset's declared `decimals` into atomic units ([`parse_amount`]):
//! no float, and a digit past `decimals` is refused rather than rounded.

use super::{Diagnostics, Dur, Secret, Settings, StoreKind};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The approver when none is named: every operator principal.
pub const OPERATOR: &str = "operator";
/// How long the approver is waited for when `approval_timeout` says nothing.
pub const DEFAULT_APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);
/// The longest an authorization is valid when `max_validity` says nothing.
pub const DEFAULT_MAX_VALIDITY: std::time::Duration = std::time::Duration::from_secs(300);
/// The most decimals an asset may declare: amounts are kept in 128 bits.
pub const MAX_DECIMALS: u8 = 36;
/// The step kinds that take `payment`: each one's transport hands a payment
/// request to `Runtime::pay_for` (RFC 0049 §3).
pub const PAYING_KINDS: &[&str] = &["http"];

/// `security.payments`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Payments {
    /// Who approves a payment the policy asks about: `operator` (the
    /// default) or one principal id.
    pub approver: Option<String>,
    /// How long the approver is waited for before the payment is declined;
    /// default 1h.
    pub approval_timeout: Option<Dur>,
    /// A payment from work that has read outside text, or that a model asked
    /// for, goes to the approver whatever its amount; default true.
    pub ask_when_tainted: Option<bool>,
    /// The longest an authorization agentd signs is valid; default 5m.
    pub max_validity: Option<Dur>,
    /// Who holds the wallet keys: the supervisor, sealed under `seal_key`
    /// (`local`, the default), or the external signer (`external`).
    pub key_custody: super::delegation::KeyCustody,
    /// 32 bytes as hex or base64, a `{{secret-file:PATH}}` reference: the
    /// key every wallet key file is sealed under. `key_custody: local` only.
    pub seal_key: Option<Secret>,
    /// The external signer, under `key_custody: external`.
    pub signer: Option<Signer>,
    pub wallets: BTreeMap<String, Wallet>,
    pub assets: BTreeMap<String, Asset>,
    pub payees: BTreeMap<String, Payee>,
}

/// `security.payments.signer`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    /// The signer's unix socket, an absolute path.
    pub socket: String,
    /// The uid the signer process runs as: the socket's peer must be it.
    pub uid: u32,
    /// The bound on one request; default 5s.
    #[serde(default)]
    pub timeout: Option<Dur>,
}

impl Signer {
    /// The signer client this names.
    #[cfg(feature = "x402")]
    pub fn signer(&self) -> crate::sec::signer::Signer {
        crate::sec::signer::Signer::new(&self.socket, self.uid, self.timeout.map(|d| d.0))
    }
}

/// `security.payments.wallets.<name>`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Wallet {
    /// The sealed key file (`key_custody: local`): made `0600` by `agentd
    /// --print-wallet-address <name>` when absent.
    pub key_file: Option<String>,
    /// The signer's key id (`key_custody: external`).
    pub key: Option<String>,
    /// The wallet's address; when given, a key of another address refuses
    /// the start.
    pub address: Option<String>,
}

/// `security.payments.assets.<name>`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Asset {
    /// The CAIP-19 asset type: `eip155:<chain>/erc20:<contract>`.
    pub asset: String,
    /// The token's decimals: `"1"` is 10^decimals atomic units.
    pub decimals: u8,
    pub limits: Limits,
    /// Above this amount the approver is asked.
    pub approve_above: Option<String>,
}

/// An asset's caps, each a decimal in its whole units.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// The most one payment may be.
    pub per_payment: Option<String>,
    /// The most one run may pay.
    pub per_run: Option<String>,
    /// The most paid in any 24 hours.
    pub per_day: Option<String>,
    /// The most paid to one payee in any 24 hours.
    pub per_payee: Option<String>,
}

/// `security.payments.payees.<name>`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Payee {
    /// The address paid (`payTo`).
    pub address: String,
    /// The assets it may be paid in; default every asset.
    pub assets: Option<Vec<String>>,
}

impl Payments {
    /// Whether the section says anything.
    pub fn declared(&self) -> bool {
        self != &Payments::default()
    }

    /// The approver.
    pub fn approver(&self) -> &str {
        self.approver.as_deref().unwrap_or(OPERATOR)
    }

    pub fn approval_timeout(&self) -> std::time::Duration {
        self.approval_timeout
            .map_or(DEFAULT_APPROVAL_TIMEOUT, |d| d.0)
    }

    pub fn max_validity(&self) -> std::time::Duration {
        self.max_validity.map_or(DEFAULT_MAX_VALIDITY, |d| d.0)
    }

    pub fn ask_when_tainted(&self) -> bool {
        self.ask_when_tainted.unwrap_or(true)
    }

    /// The signer that holds the wallet keys, under `key_custody: external`.
    pub fn external(&self) -> Option<&Signer> {
        match self.key_custody {
            super::delegation::KeyCustody::External => self.signer.as_ref(),
            super::delegation::KeyCustody::Local => None,
        }
    }

    /// Whether any payment could be asked of the approver.
    pub fn may_ask(&self) -> bool {
        self.ask_when_tainted() || self.assets.values().any(|a| a.approve_above.is_some())
    }
}

/// `text`, a decimal in whole units, as atomic units at `decimals`:
/// exactly, or refused — a sign, an exponent, a digit past `decimals`, or a
/// value past 128 bits.
pub fn parse_amount(text: &str, decimals: u8) -> Result<u128, String> {
    let bad = || {
        format!(
            "{text:?} is not an amount (a decimal like \"0.25\", at most {decimals} digits after the point)"
        )
    };
    let t = text.trim();
    let (whole, frac) = match t.split_once('.') {
        Some((w, f)) => (w, f),
        None => (t, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || (t.contains('.') && frac.is_empty())
        || frac.len() > usize::from(decimals)
    {
        return Err(bad());
    }
    let mut v: u128 = 0;
    let digits = whole.bytes().chain(frac.bytes()).chain(std::iter::repeat_n(
        b'0',
        usize::from(decimals) - frac.len(),
    ));
    for d in digits {
        v = v
            .checked_mul(10)
            .and_then(|v| v.checked_add(u128::from(d - b'0')))
            .ok_or_else(bad)?;
    }
    Ok(v)
}

/// An atomic amount as a decimal in whole units at `decimals`: `250000` at 6
/// is `"0.25"`.
pub fn format_amount(atomic: u128, decimals: u8) -> String {
    let s = atomic.to_string();
    let d = usize::from(decimals);
    if d == 0 {
        return s;
    }
    let s = format!("{s:0>width$}", width = d + 1);
    let (whole, frac) = s.split_at(s.len() - d);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    }
}

/// A server's `amount`: atomic units, a decimal of digits only, no leading
/// zero, at most 128 bits.
pub fn parse_atomic(text: &str) -> Result<u128, String> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(format!("{text:?} is not an amount in atomic units"));
    }
    text.parse()
        .map_err(|_| format!("{text:?} is past 128 bits of atomic units"))
}

/// A name the operator gives a wallet, an asset or a payee.
pub fn is_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// An EVM address: `0x` and 40 hex digits (EIP-55's checksum held when mixed).
fn evm_address(s: &str) -> Result<(), String> {
    #[cfg(feature = "x402")]
    {
        crate::payments::evm::Address::parse(s).map(|_| ())
    }
    #[cfg(not(feature = "x402"))]
    {
        let ok = s
            .strip_prefix("0x")
            .is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
        if ok {
            Ok(())
        } else {
            Err(format!("{s:?} is not an address (0x and 40 hex digits)"))
        }
    }
}

/// An ERC-20 asset's CAIP-19 id: `eip155:<chain>/erc20:<contract>`.
fn erc20_asset(s: &str) -> Result<(), String> {
    let bad = || {
        format!(
            "{s:?} is not an ERC-20 asset's CAIP-19 id (eip155:<chain id>/erc20:<contract>); unit 1 of RFC 0049 pays on EVM chains"
        )
    };
    let (chain, asset) = s.split_once('/').ok_or_else(bad)?;
    let (ns, id) = chain.split_once(':').ok_or_else(bad)?;
    let chain_ok = ns == "eip155"
        && !id.is_empty()
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_digit())
        && !id.starts_with('0');
    let (ans, contract) = asset.split_once(':').ok_or_else(bad)?;
    if !chain_ok || ans != "erc20" {
        return Err(bad());
    }
    evm_address(contract).map_err(|e| format!("{s:?}: {e}"))
}

/// Every check RFC 0049 makes at load on `security.payments`.
pub(super) fn validate(s: &Settings, d: &mut Diagnostics) {
    let p = &s.security.payments;
    if !p.declared() {
        return;
    }
    let mut err = |m: String| d.errors.push(m);
    if !cfg!(feature = "x402") {
        err("security.payments needs the `x402` feature (build with --features x402)".into());
        return;
    }
    // ADR 0003: no payment without every part of the policy.
    if p.wallets.is_empty() || p.assets.is_empty() || p.payees.is_empty() {
        err("security.payments names no wallet, no asset or no payee: an allow-list that is empty pays no one — name at least one of each (RFC 0049 §4)".into());
    }
    // Every payment an audit record (ADR 0003): the chain must be there.
    if s.observability.audit.chain.is_none() {
        err("security.payments needs observability.audit.chain: every payment is a record in the hash-chained audit log, written before the payment is sent (RFC 0049 D10)".into());
    }
    // A payment whose outcome is unknown is never made twice: the ledger
    // must outlive the process.
    if s.store.kind == StoreKind::Memory {
        err("security.payments needs a durable store (store.kind file, mcp or http): the ledger of what was paid must survive a restart, or a payment whose outcome is unknown could be made again (RFC 0049 D9)".into());
    }
    let approver = p.approver();
    if approver != OPERATOR
        && (!super::companies::is_principal_id(approver) || approver.contains(['*', '?']))
    {
        err(format!(
            "security.payments.approver: {approver:?} is neither `operator` nor a principal id (`<role>:<id>`); a glob or a role names more than the one person a payment's approval is asked of"
        ));
    }
    if let Some(t) = p.approval_timeout
        && (t.0 < std::time::Duration::from_secs(1)
            || t.0 > std::time::Duration::from_secs(7 * 86400))
    {
        err("security.payments.approval_timeout: between 1s and 7d".into());
    }
    if let Some(t) = p.max_validity
        && (t.0 < std::time::Duration::from_secs(10) || t.0 > std::time::Duration::from_secs(3600))
    {
        err("security.payments.max_validity: between 10s and 1h".into());
    }
    // Custody.
    use super::delegation::KeyCustody;
    match (p.key_custody, &p.signer) {
        (KeyCustody::External, None) => err(
            "security.payments.key_custody external needs security.payments.signer {socket, uid}".into(),
        ),
        (KeyCustody::Local, Some(_)) => err(
            "security.payments.signer is read under key_custody: external alone — say key_custody: external, or drop the signer".into(),
        ),
        (KeyCustody::External, Some(sg)) => {
            if !std::path::Path::new(&sg.socket).is_absolute() {
                err(format!(
                    "security.payments.signer.socket {:?} is not an absolute path to the signer's unix socket",
                    sg.socket
                ));
            }
            // SAFETY: geteuid has no failure mode.
            if sg.uid == unsafe { libc::geteuid() } {
                d.warnings.push(format!(
                    "security.payments.signer.uid {} is agentd's own: the peer check cannot tell the signer from a process of agentd's uid — run the signer as another uid",
                    sg.uid
                ));
            }
            if let Some(t) = sg.timeout
                && (t.0.is_zero() || t.0 > std::time::Duration::from_secs(60))
            {
                d.errors
                    .push("security.payments.signer.timeout: between 1ms and 60s".into());
            }
            if p.seal_key.is_some() {
                d.errors.push("security.payments.seal_key: under key_custody external the signer holds every wallet key — give no seal key".into());
            }
        }
        (KeyCustody::Local, None) => {
            if p.seal_key.is_none() && !p.wallets.is_empty() {
                d.errors.push("security.payments.seal_key: under key_custody local every wallet key is sealed under it — a {{secret-file:PATH}} reference to 32 bytes in hex or base64".into());
            }
        }
    }
    let mut err = |m: String| d.errors.push(m);
    if let Some(Secret(sk)) = &p.seal_key {
        let refs = crate::sec::secret::refs_of(sk);
        let whole = refs.len() == 1
            && sk.trim() == format!("{{{{{}}}}}", refs[0])
            && refs[0].starts_with("secret-file:");
        if !whole {
            err("security.payments.seal_key: a single {{secret-file:PATH}} reference — never an inline key, and never the environment".into());
        }
    }
    let external = p.key_custody == KeyCustody::External;
    for (name, w) in &p.wallets {
        let at = format!("security.payments.wallets.{name}");
        if !is_name(name) {
            err(format!(
                "{at}: a wallet's name is 1 to 32 of a-z, 0-9, - and _"
            ));
        }
        match (external, &w.key_file, &w.key) {
            (false, Some(f), None) => {
                if !std::path::Path::new(f).is_absolute() {
                    err(format!("{at}.key_file {f:?} is not an absolute path"));
                }
            }
            (false, _, _) => err(format!(
                "{at}: under key_custody local a wallet names its sealed key_file, and no signer key"
            )),
            (true, None, Some(k)) if !k.trim().is_empty() => {}
            (true, _, _) => err(format!(
                "{at}: under key_custody external a wallet names its signer key id (key), and no key_file"
            )),
        }
        if let Some(a) = &w.address
            && let Err(e) = evm_address(a)
        {
            err(format!("{at}.address: {e}"));
        }
    }
    for (name, a) in &p.assets {
        let at = format!("security.payments.assets.{name}");
        if !is_name(name) {
            err(format!(
                "{at}: an asset's name is 1 to 32 of a-z, 0-9, - and _"
            ));
        }
        if let Err(e) = erc20_asset(&a.asset) {
            err(format!("{at}.asset: {e}"));
        }
        if a.decimals > MAX_DECIMALS {
            err(format!("{at}.decimals: at most {MAX_DECIMALS}"));
        }
        let amount = |k: &str, v: &Option<String>, err: &mut dyn FnMut(String)| {
            if let Some(v) = v
                && let Err(e) = parse_amount(v, a.decimals)
            {
                err(format!("{at}.{k}: {e}"));
            }
        };
        let l = &a.limits;
        for (k, v) in [
            ("per_run", &l.per_run),
            ("per_day", &l.per_day),
            ("per_payee", &l.per_payee),
        ] {
            if v.is_none() {
                err(format!(
                    "{at}.limits.{k} is not set: every asset is capped per run, per day and per payee (ADR 0003)"
                ));
            }
        }
        amount("limits.per_payment", &l.per_payment, &mut err);
        amount("limits.per_run", &l.per_run, &mut err);
        amount("limits.per_day", &l.per_day, &mut err);
        amount("limits.per_payee", &l.per_payee, &mut err);
        amount("approve_above", &a.approve_above, &mut err);
        if p.assets
            .values()
            .filter(|x| x.asset.eq_ignore_ascii_case(&a.asset))
            .count()
            > 1
        {
            err(format!(
                "{at}.asset {:?} is named by another asset too",
                a.asset
            ));
        }
    }
    for (name, payee) in &p.payees {
        let at = format!("security.payments.payees.{name}");
        if !is_name(name) {
            err(format!(
                "{at}: a payee's name is 1 to 32 of a-z, 0-9, - and _"
            ));
        }
        if let Err(e) = evm_address(&payee.address) {
            err(format!("{at}.address: {e}"));
        }
        for asset in payee.assets.iter().flatten() {
            if !p.assets.contains_key(asset) {
                err(format!(
                    "{at}.assets: {asset:?} is not a security.payments.assets entry"
                ));
            }
        }
    }
    // Asked of a person who can be asked.
    if p.may_ask() && s.a2a.listen.is_none() {
        d.warnings.push("security.payments asks its approver about some payments (approve_above, or ask_when_tainted), and no A2A listener serves (a2a.listen): nobody can be asked, so each such payment is refused unsent".into());
    }
}

/// The step's `payment`, at any depth of `w`: `(step id, the field)`.
pub fn paying_steps(w: &crate::engine::model::Workflow) -> Vec<(String, Value)> {
    fn walk(steps: &BTreeMap<String, crate::engine::model::Step>, out: &mut Vec<(String, Value)>) {
        for (id, st) in steps {
            if PAYING_KINDS.contains(&st.kind.as_str())
                && let Some(p) = st.field("payment")
            {
                out.push((id.clone(), p.clone()));
            }
            for body in st.body.iter().chain(st.branches.values()) {
                walk(&body.steps, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&w.steps, &mut out);
    out
}

/// Whether a workflow definition, as JSON, has a paying step at any depth.
pub fn pays_json(def: &Value) -> bool {
    match def {
        Value::Object(o) => {
            (o.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| PAYING_KINDS.contains(&k))
                && o.contains_key("payment"))
                || o.values().any(pays_json)
        }
        Value::Array(a) => a.iter().any(pays_json),
        _ => false,
    }
}

/// The refusal of a paying step in a definition the operator's own
/// configuration did not write.
pub fn not_configured(workflow: &str, step: &str) -> String {
    format!(
        "workflow {workflow:?} step {step:?}: an http step's payment is declared only in the operator's own configuration — never in an instruction document's workflow, nor in a definition workflow.create or workflow.update stored (RFC 0049 D2)"
    )
}

/// The refusal of an instruction document's workflow that would pay.
pub fn document_pays(workflow: &str) -> String {
    format!(
        "workflow {workflow:?}: an instruction document's workflow has an http step with a payment — a served document configures no payment (RFC 0042; RFC 0049 D2), only the operator's own configuration declares a paying step"
    )
}

/// A step's `payment`, read against the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPayment {
    pub wallet: String,
    /// The assets its `max` names, each with its most in atomic units.
    pub max: Vec<(String, u128)>,
    /// Whether every payment of the step asks the approver.
    pub approve: bool,
}

/// Read a step's `payment` against the policy `p`.
pub fn step_payment(p: &Payments, v: &Value) -> Result<StepPayment, String> {
    let o = v.as_object().ok_or("payment is an object")?;
    let wallet = o
        .get("wallet")
        .and_then(Value::as_str)
        .ok_or("payment.wallet is not set")?;
    if !p.wallets.contains_key(wallet) {
        return Err(format!(
            "payment.wallet {wallet:?} is not a security.payments.wallets entry"
        ));
    }
    let maxes: Vec<&str> = match o.get("max") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Err("payment.max is not set".into()),
    };
    let mut max = Vec::new();
    for m in maxes {
        let (amount, asset) = m.trim().split_once(' ').ok_or_else(|| {
            format!("payment.max {m:?} is not \"<amount> <asset>\" (e.g. \"0.05 usdc-base\")")
        })?;
        let a = p.assets.get(asset.trim()).ok_or_else(|| {
            format!("payment.max {m:?}: {asset:?} is not a security.payments.assets entry")
        })?;
        let atomic = parse_amount(amount, a.decimals).map_err(|e| format!("payment.max: {e}"))?;
        if max.iter().any(|(x, _)| x == asset.trim()) {
            return Err(format!("payment.max names {asset:?} twice"));
        }
        max.push((asset.trim().to_string(), atomic));
    }
    Ok(StepPayment {
        wallet: wallet.to_string(),
        max,
        approve: o.get("approve").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// What a workflow's paying steps name that the policy does not have, at
/// any depth — or, without the feature, every paying step.
pub fn workflow_refusals(s: &Settings, w: &crate::engine::model::Workflow) -> Vec<String> {
    let mut out = Vec::new();
    for (id, field) in paying_steps(w) {
        let at = format!("workflow {:?} step {id:?}", w.name);
        if !cfg!(feature = "x402") {
            out.push(format!(
                "{at}: an http step's payment needs the `x402` feature (build with --features x402)"
            ));
            continue;
        }
        if !s.security.payments.declared() {
            out.push(format!(
                "{at}: an http step's payment needs security.payments, the spend policy it pays within (RFC 0049 §4)"
            ));
            continue;
        }
        if let Err(e) = step_payment(&s.security.payments, &field) {
            out.push(format!("{at}: {e}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amounts read exactly in atomic units, and print back.
    #[test]
    fn amounts_are_exact() {
        assert_eq!(parse_amount("0.25", 6), Ok(250_000));
        assert_eq!(parse_amount("1", 6), Ok(1_000_000));
        assert_eq!(parse_amount("20.000001", 6), Ok(20_000_001));
        assert_eq!(parse_amount("0", 6), Ok(0));
        assert_eq!(parse_amount("5", 0), Ok(5));
        assert_eq!(parse_amount("1.5", 18), Ok(1_500_000_000_000_000_000));
        for bad in [
            "0.0000001",
            "-1",
            "+1",
            "1e6",
            "1.",
            ".5",
            "",
            " ",
            "0x10",
            "1,5",
        ] {
            assert!(parse_amount(bad, 6).is_err(), "{bad:?}");
        }
        assert!(parse_amount(&"9".repeat(40), 0).is_err(), "past 128 bits");
        for (atomic, d, s) in [
            (250_000, 6, "0.25"),
            (1_000_000, 6, "1"),
            (1, 6, "0.000001"),
            (0, 6, "0"),
            (12, 0, "12"),
        ] {
            assert_eq!(format_amount(atomic, d), s);
            assert_eq!(parse_amount(s, d), Ok(atomic));
        }
        assert_eq!(parse_atomic("10000"), Ok(10_000));
        for bad in ["", "01", "1.0", "-5", "0x10", &"9".repeat(40)] {
            assert!(parse_atomic(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_atomic("0"), Ok(0));
    }

    /// The configuration a payment needs, as `edit` rewrites it, loaded:
    /// the refusal, or `""`.
    #[cfg(feature = "x402")]
    fn load_error(edit: &dyn Fn(String) -> String) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("seal"), "07".repeat(32)).unwrap();
        let yaml = edit(BASE.to_string()).replace("__D__", &dir.path().display().to_string());
        let path = dir.path().join("c.yaml");
        std::fs::write(&path, yaml).unwrap();
        crate::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    #[cfg(feature = "x402")]
    const BASE: &str = r#"agent: {name: payer, preflight: never}
store: {kind: file, file: {path: "__D__/state"}}
observability: {audit: {chain: {path: "__D__/audit.log"}}}
security:
  payments:
    seal_key: "{{secret-file:__D__/seal}}"
    wallets: {ops: {key_file: "__D__/ops.key"}}
    assets:
      usdc:
        asset: "eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
        decimals: 6
        limits: {per_payment: "0.10", per_run: "1", per_day: "5", per_payee: "2"}
    payees: {api: {address: "0x209693Bc6afc0C5328bA36FaF03C514EF312287C"}}
workflows:
  - name: buy
    steps:
      s: {kind: manual}
      get: {kind: http, depends_on: [s], url: "https://api.example/data", payment: {wallet: ops, max: "0.05 usdc"}}
      done: {kind: finish, depends_on: [get]}
"#;

    /// Every part of ADR 0003 is there, or the configuration is refused
    /// saying which is not: the audit chain, a ledger that outlives the
    /// process, a cap per run, day and payee, a sealed key behind a mounted
    /// secret, one approver who is a person, payees whose address is what
    /// the operator meant — and a step that names what the policy has.
    #[cfg(feature = "x402")]
    #[test]
    fn a_spend_policy_missing_a_part_is_refused() {
        assert_eq!(load_error(&|y| y), "");
        for (edit, says) in [
            (
                Box::new(|y: String| {
                    y.replace(
                        "store: {kind: file, file: {path: \"__D__/state\"}}",
                        "store: {kind: memory}",
                    )
                }) as Box<dyn Fn(String) -> String>,
                "needs a durable store",
            ),
            (
                Box::new(|y: String| {
                    y.replace(
                        "observability: {audit: {chain: {path: \"__D__/audit.log\"}}}\n",
                        "",
                    )
                }),
                "needs observability.audit.chain",
            ),
            (
                Box::new(|y: String| y.replace(", per_day: \"5\"", "")),
                "limits.per_day is not set",
            ),
            (
                Box::new(|y: String| y.replace("\"{{secret-file:__D__/seal}}\"", "\"0707\"")),
                "a single {{secret-file:PATH}} reference",
            ),
            (
                Box::new(|y: String| {
                    y.replace("\"{{secret-file:__D__/seal}}\"", "\"{{secret:SEAL}}\"")
                }),
                "a single {{secret-file:PATH}} reference",
            ),
            (
                Box::new(|y: String| {
                    y.replace("  payments:\n", "  payments:\n    approver: \"user:*\"\n")
                }),
                "the one person",
            ),
            (
                Box::new(|y: String| {
                    y.replace("  payments:\n", "  payments:\n    approver: finance\n")
                }),
                "the one person",
            ),
            (
                Box::new(|y: String| {
                    y.replace(
                        "0x209693Bc6afc0C5328bA36FaF03C514EF312287C",
                        "0x209693bc6afc0C5328bA36FaF03C514EF312287C",
                    )
                }),
                "EIP-55 checksum",
            ),
            (
                Box::new(|y: String| y.replace("eip155:8453/erc20:", "eip155:8453/slip44:")),
                "not an ERC-20 asset",
            ),
            (
                Box::new(|y: String| y.replace("max: \"0.05 usdc\"", "max: \"0.05 eurc\"")),
                "\"eurc\" is not a security.payments.assets entry",
            ),
            (
                Box::new(|y: String| y.replace("max: \"0.05 usdc\"", "max: \"0.0000001 usdc\"")),
                "at most 6 digits after the point",
            ),
            (
                Box::new(|y: String| y.replace("wallet: ops", "wallet: other")),
                "payment.wallet \"other\"",
            ),
            (
                Box::new(|y: String| y.replace("max: \"0.05 usdc\"", "max: \"{{ inputs.max }}\"")),
                "payment is literal",
            ),
            (
                Box::new(|y: String| {
                    y.replace("{kind: manual}", "{kind: manual}\n      note: {kind: noop, depends_on: [s], payment: {wallet: ops, max: \"0.05 usdc\"}}")
                }),
                "unknown field \"payment\" for kind \"noop\"",
            ),
            (
                Box::new(|y: String| {
                    y.replace("    wallets: {ops: {key_file: \"__D__/ops.key\"}}\n", "    key_custody: external\n    wallets: {ops: {key_file: \"__D__/ops.key\"}}\n")
                }),
                "key_custody external needs security.payments.signer",
            ),
        ] {
            let e = load_error(&*edit);
            assert!(e.contains(says), "want {says:?}, got: {e}");
        }
    }

    /// Paying steps are found at any depth, in the definition and its JSON.
    #[test]
    fn paying_steps_are_found_at_any_depth() {
        let def = serde_json::json!({"name": "w", "steps": {
            "s": {"kind": "manual"},
            "each": {"kind": "foreach", "over": [1], "depends_on": ["s"], "body": {"steps": {
                "pay": {"kind": "http", "url": "https://x/y", "payment": {"wallet": "ops", "max": "0.1 usdc"}}}}},
            "f": {"kind": "finish", "depends_on": ["each"]}}});
        let w = crate::engine::model::parse_workflow(&def).unwrap();
        let found = paying_steps(&w);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(pays_json(&def));
        assert!(!pays_json(
            &serde_json::json!({"steps": {"a": {"kind": "http", "url": "https://x"}}})
        ));
    }
}
