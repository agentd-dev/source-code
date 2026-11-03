// SPDX-License-Identifier: AGPL-3.0-only
//! Allowances (RFC 0057): what agentd may spend, on what, over what period,
//! and who approves — the limits a person means, stated once, above the
//! payment rails.
//!
//! This module is the engine both rails share, whatever the build: an
//! allowance read from `security.payments.allowances.<id>` ([`Allowance`]),
//! the calendar period a payment falls in ([`period_start`]), the judgement
//! of one charge against it ([`judge`]), and the ledger of what a Company's
//! operations were charged ([`Spending`]). An x402 payment's record is the
//! payment ledger's (`payments::ledger`); [`Spending`] counts both.
//!
//! **Amounts.** Every amount of an allowance is a decimal in its currency,
//! held exactly as an integer of [`SCALE`] decimals — no float, and a digit
//! past them refused. One whole unit of a token whose asset names the
//! currency is one unit of it, so a token amount converts exactly while its
//! asset has at most [`SCALE`] decimals (load refuses more). A charge is
//! written into a record as a decimal string in the allowance's currency
//! (`"20.5"`), with the currency beside it.
//!
//! **Periods** are UTC calendar periods: a day from 00:00, an ISO week from
//! Monday, a month from the 1st, a quarter from January, April, July or
//! October, a year from January 1st. `once` is the allowance's whole life.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::settings::payments::{self as cfg, format_amount, parse_amount};
use crate::state::{Durable, Kind};

/// The decimals an allowance's amounts are held at.
pub const SCALE: u8 = 18;
/// Where a Company operation's charges are kept, in the store.
pub const PREFIX: &str = "spending/charges/";
const DAY_MS: u64 = 86_400_000;

/// An allowance's period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Per {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Once,
}

impl Per {
    pub fn parse(s: &str) -> Option<Per> {
        Some(match s {
            "day" => Per::Day,
            "week" => Per::Week,
            "month" => Per::Month,
            "quarter" => Per::Quarter,
            "year" => Per::Year,
            "once" => Per::Once,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Per::Day => "day",
            Per::Week => "week",
            Per::Month => "month",
            Per::Quarter => "quarter",
            Per::Year => "year",
            Per::Once => "once",
        }
    }

    /// How a reason says it: `per month`, or `in all`.
    fn said(self) -> String {
        match self {
            Per::Once => "in all".into(),
            p => format!("per {}", p.as_str()),
        }
    }
}

/// Days since 1970-01-01 of a civil date (Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The start of the UTC calendar period `now_ms` falls in, epoch
/// milliseconds; `None` for `once`, which never renews.
pub fn period_start(per: Per, now_ms: u64) -> Option<u64> {
    let days = (now_ms / DAY_MS) as i64;
    let (y, m, _) = crate::obs::log::civil_from_days(days);
    let start_days = match per {
        Per::Once => return None,
        Per::Day => days,
        // 1970-01-01 was a Thursday: Monday is 3 days before it.
        Per::Week => days - (days + 3).rem_euclid(7),
        Per::Month => days_from_civil(y, m, 1),
        Per::Quarter => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1),
        Per::Year => days_from_civil(y, 1, 1),
    };
    Some(start_days.max(0) as u64 * DAY_MS)
}

/// The start of the period after the one that starts at `start`.
pub fn period_end(per: Per, start: u64) -> Option<u64> {
    let days = (start / DAY_MS) as i64;
    let (y, m, _) = crate::obs::log::civil_from_days(days);
    let next = |y: i64, m: i64| {
        if m > 12 {
            days_from_civil(y + 1, m - 12, 1)
        } else {
            days_from_civil(y, m, 1)
        }
    };
    let end_days = match per {
        Per::Once => return None,
        Per::Day => days + 1,
        Per::Week => days + 7,
        Per::Month => next(y, m + 1),
        Per::Quarter => next(y, m + 3),
        Per::Year => days_from_civil(y + 1, 1, 1),
    };
    Some(end_days as u64 * DAY_MS)
}

/// An epoch-milliseconds instant in RFC 3339, to the second.
pub fn rfc3339(ms: u64) -> String {
    let days = (ms / DAY_MS) as i64;
    let (y, m, d) = crate::obs::log::civil_from_days(days);
    let s = (ms % DAY_MS) / 1000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Whether `s` is an ISO 4217 code's shape: three upper-case letters.
pub fn is_currency(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// A token amount in atomic units at `decimals` as an amount of the
/// currency its asset names, at [`SCALE`]: exact, or `None` past 128 bits
/// or past [`SCALE`] decimals.
pub fn to_currency(atomic: u128, decimals: u8) -> Option<u128> {
    let shift = SCALE.checked_sub(decimals)?;
    10u128
        .checked_pow(u32::from(shift))
        .and_then(|f| atomic.checked_mul(f))
}

/// An amount of a currency at [`SCALE`] as atomic units of a token at
/// `decimals`, rounded down: the most of the token that is within it.
pub fn from_currency(amount: u128, decimals: u8) -> u128 {
    match SCALE.checked_sub(decimals) {
        Some(shift) => amount / 10u128.pow(u32::from(shift)),
        None => amount,
    }
}

/// An allowance, read: its amounts at [`SCALE`], its window in epoch
/// milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Allowance {
    pub id: String,
    pub title: Option<String>,
    pub currency: String,
    pub wallets: Option<Vec<String>>,
    pub max: u128,
    pub budget: u128,
    pub per: Per,
    pub times: Option<u32>,
    pub payees: Option<Vec<String>>,
    pub categories: Vec<String>,
    pub from_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub approve_above: Option<u128>,
    pub approver: Option<String>,
}

impl Allowance {
    /// `security.payments.allowances.<id>`, checked at load, read.
    pub fn from_settings(id: &str, a: &cfg::Allowance) -> Result<Allowance, String> {
        let at = format!("security.payments.allowances.{id}");
        let amount = |k: &str, v: &Option<String>| -> Result<Option<u128>, String> {
            v.as_deref()
                .map(|v| parse_amount(v, SCALE).map_err(|e| format!("{at}.{k}: {e}")))
                .transpose()
        };
        let time = |k: &str, v: &Option<String>| -> Result<Option<u64>, String> {
            v.as_deref()
                .map(|v| {
                    crate::obs::log::rfc3339_ms(v)
                        .ok_or_else(|| format!("{at}.{k}: {v:?} is not an RFC 3339 instant"))
                })
                .transpose()
        };
        let per = match a.per.as_deref() {
            None => Per::Once,
            Some(p) => Per::parse(p).ok_or_else(|| {
                format!("{at}.per: {p:?} is not day, week, month, quarter, year or once")
            })?,
        };
        Ok(Allowance {
            id: id.to_string(),
            title: a.title.clone(),
            currency: a.currency.clone(),
            wallets: a.wallets.clone(),
            max: amount("max", &a.max)?.ok_or_else(|| format!("{at}.max is not set"))?,
            budget: amount("budget", &a.budget)?
                .ok_or_else(|| format!("{at}.budget is not set"))?,
            per,
            times: a.times,
            payees: a.payees.clone(),
            categories: a.categories.clone(),
            from_ms: time("from", &a.from)?,
            until_ms: time("until", &a.until)?,
            approve_above: amount("approve_above", &a.approve_above)?,
            approver: a.approver.clone(),
        })
    }

    /// Every allowance of `p` that reads; one that does not is left out
    /// (load refuses it, so none is).
    pub fn all(p: &cfg::Payments) -> BTreeMap<String, Allowance> {
        p.allowances
            .iter()
            .filter_map(|(id, a)| Some((id.clone(), Allowance::from_settings(id, a).ok()?)))
            .collect()
    }

    /// `amount` with the currency: `"20.5 GBP"`.
    pub fn show(&self, amount: u128) -> String {
        format!("{} {}", format_amount(amount, SCALE), self.currency)
    }

    /// The start of the period `now_ms` falls in; 0 for `once`.
    pub fn since(&self, now_ms: u64) -> u64 {
        period_start(self.per, now_ms).unwrap_or(0)
    }

    /// Whether a charge of this allowance at `at_ms` still counts at
    /// `now_ms`: it fell in the current period.
    pub fn holds(&self, at_ms: u64, now_ms: u64) -> bool {
        at_ms >= self.since(now_ms)
    }
}

/// What counts against an allowance in its current period.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// In the allowance's currency, at [`SCALE`].
    pub spent: u128,
    pub payments: u32,
}

impl Usage {
    pub fn add(&mut self, amount: u128) {
        self.spent = self.spent.saturating_add(amount);
        self.payments = self.payments.saturating_add(1);
    }
}

/// A charge of `charge` (at [`SCALE`]) to `payee` held to `a`, with `usage`
/// already counted, at `now_ms`: whether it is above `approve_above`, or
/// every limit it is past, each reason naming the allowance.
pub fn judge(
    a: &Allowance,
    payee: &str,
    charge: u128,
    usage: Usage,
    now_ms: u64,
) -> Result<bool, Vec<String>> {
    let mut why = Vec::new();
    let at = format!("allowance {}", a.id);
    if !a.categories.is_empty() {
        why.push(format!(
            "{at}: rail-cannot-enforce: categories — no rail agentd pays on can hold a payment to a kind of purchase ({})",
            a.categories.join(", ")
        ));
    }
    if let Some(from) = a.from_ms
        && now_ms < from
    {
        why.push(format!("{at}: from {}, not yet", rfc3339(from)));
    }
    if let Some(until) = a.until_ms
        && now_ms >= until
    {
        why.push(format!("{at}: until {}, past", rfc3339(until)));
    }
    if let Some(list) = &a.payees
        && !list.iter().any(|p| p == payee)
    {
        why.push(format!("{at}: payee {payee} is not one of its payees"));
    }
    if charge > a.max {
        why.push(format!(
            "{at}: max {}, this payment {}",
            a.show(a.max),
            format_amount(charge, SCALE)
        ));
    }
    if usage.spent.checked_add(charge).is_none_or(|t| t > a.budget) {
        why.push(format!(
            "{at}: budget {} {}, {} spent, this payment {}",
            a.show(a.budget),
            a.per.said(),
            format_amount(usage.spent, SCALE),
            format_amount(charge, SCALE)
        ));
    }
    if let Some(t) = a.times
        && usage.payments >= t
    {
        why.push(format!(
            "{at}: times {t} {}, {} made",
            a.per.said(),
            usage.payments
        ));
    }
    if why.is_empty() {
        Ok(a.approve_above.is_some_and(|t| charge > t))
    } else {
        Err(why)
    }
}

/// Where a Company operation's charge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeState {
    /// The confirm is on its way.
    Sent,
    /// The Company took the confirm.
    Confirmed,
    /// Whether the Company took it is not known.
    Unknown,
    /// The Company did not take it: nothing was bought.
    Refused,
    /// agentd cancelled the operation and the Company confirmed it.
    Credited,
}

impl ChargeState {
    pub fn counts(self) -> bool {
        matches!(
            self,
            ChargeState::Sent | ChargeState::Confirmed | ChargeState::Unknown
        )
    }
}

/// One Company operation's charge to an allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charge {
    /// The company entry and the operation id.
    pub company: String,
    pub operation: String,
    pub allowance: String,
    pub currency: String,
    /// A decimal in `currency`.
    pub amount: String,
    pub state: ChargeState,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
}

impl Charge {
    fn id(&self) -> String {
        charge_id(&self.company, &self.operation)
    }

    fn amount(&self) -> u128 {
        parse_amount(&self.amount, SCALE).unwrap_or(u128::MAX)
    }
}

fn charge_id(company: &str, operation: &str) -> String {
    format!("{company}.{operation}")
}

/// What the x402 ledger counts against an allowance: its records that name
/// it, in its currency, from `since`, that count at `now_ms`.
pub trait Counted: Send + Sync {
    fn usage(&self, allowance: &str, currency: &str, since: u64, now_ms: u64) -> Usage;
}

/// The Company charges, by id, held under one lock: a check and the charge
/// it allows are one step, on either rail.
pub struct Charges(BTreeMap<String, Charge>);

/// What both rails have charged each allowance: the Company operations'
/// charges, kept here, and the x402 ledger's records, counted through
/// [`Counted`].
pub struct Spending {
    durable: Arc<Durable>,
    charges: Mutex<Charges>,
    x402: OnceLock<Arc<dyn Counted>>,
    /// The allowances as the settings say now: set at start and by every
    /// reload, read under the charges' lock when a Company charge is
    /// taken, so the charge is judged against what is in force then.
    live: Mutex<BTreeMap<String, Allowance>>,
}

/// What [`Spending::charge`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charged {
    /// A new charge is kept; whether it is above `approve_above`.
    New(bool),
    /// The operation's charge from a confirm whose outcome is not known
    /// yet — `sent` or `unknown` — is kept as it is: the confirm is sent
    /// again, and the charge still counts once.
    Held,
}

impl Spending {
    /// Every charge kept in the store, read.
    pub fn open(durable: Arc<Durable>) -> Result<Spending, String> {
        let list = durable
            .list_prefix(Kind::Cred, PREFIX)
            .map_err(|e| format!("the spending ledger: {e}"))?;
        let mut charges = BTreeMap::new();
        for ks in list {
            let Some((_, id)) =
                crate::store::parse_key(durable.prefix(), durable.instance(), &ks.key)
            else {
                continue;
            };
            if !id.starts_with(PREFIX) {
                continue;
            }
            let Some(env) = durable
                .get(Kind::Cred, id)
                .map_err(|e| format!("the spending ledger: {id}: {e}"))?
            else {
                continue;
            };
            let c: Charge = serde_json::from_value(env.state).map_err(|e| {
                format!("the spending ledger: {id} is not a charge ({e}); agentd will not spend while it cannot read what it spent")
            })?;
            charges.insert(c.id(), c);
        }
        Ok(Spending {
            durable,
            charges: Mutex::new(Charges(charges)),
            x402: OnceLock::new(),
            live: Mutex::new(BTreeMap::new()),
        })
    }

    /// The x402 ledger, counted beside the charges from here on.
    pub fn count_x402(&self, x402: Arc<dyn Counted>) {
        let _ = self.x402.set(x402);
    }

    /// The charges, locked: what a check and the record it allows hold
    /// between them. Never taken while the x402 ledger's lock is held.
    pub fn hold(&self) -> MutexGuard<'_, Charges> {
        self.charges.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What counts against `a` at `now_ms`, `held` the charges, leaving out
    /// the charge `except` names (the one being made again).
    pub fn usage(
        &self,
        held: &Charges,
        a: &Allowance,
        now_ms: u64,
        except: Option<(&str, &str)>,
    ) -> Usage {
        let since = a.since(now_ms);
        let skip = except.map(|(c, o)| charge_id(c, o));
        let mut u = Usage::default();
        for c in held.0.values() {
            if c.allowance == a.id
                && c.currency == a.currency
                && c.state.counts()
                && c.at_ms >= since
                && skip.as_deref() != Some(c.id().as_str())
            {
                u.add(c.amount());
            }
        }
        if let Some(x) = self.x402.get() {
            let more = x.usage(&a.id, &a.currency, since, now_ms);
            u.spent = u.spent.saturating_add(more.spent);
            u.payments = u.payments.saturating_add(more.payments);
        }
        u
    }

    /// The allowances in force from now on: every reload sets them.
    pub fn set_allowances(&self, all: BTreeMap<String, Allowance>) {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = all;
    }

    /// One allowance in force from now on, beside the others.
    pub fn set_allowance(&self, a: Allowance) {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(a.id.clone(), a);
    }

    /// The allowance `id` as it is in force now, or why there is none.
    pub fn allowance(&self, id: &str) -> Result<Allowance, String> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "allowance {id}: it is no longer a security.payments.allowances entry, so nothing is charged to it"
                )
            })
    }

    /// Why `operation` at `company` is not charged again: its charge is
    /// confirmed — the Company took a confirm of it — so a confirm of the
    /// same operation id is never sent again (RFC 0057 §4.5).
    pub fn confirmed_already(&self, company: &str, operation: &str) -> Option<String> {
        self.charge_of(company, operation)
            .filter(|c| c.state == ChargeState::Confirmed)
            .map(|c| {
                format!(
                    "company {company}: operation {operation} was confirmed and charged already ({} {} to allowance {}) — agentd never confirms an operation twice; nothing was sent",
                    c.amount, c.currency, c.allowance
                )
            })
    }

    /// The charge of `amount` for `operation` at `company` to the allowance
    /// `id` as it is in force now, if it is within it at `now_ms`, kept
    /// before the confirm is sent: one step under the lock, so two confirms
    /// never pass a budget together. A charge of the operation that counts
    /// already is never written over nor left out: a confirmed one refuses,
    /// one of unknown outcome is kept as it is ([`Charged::Held`]).
    pub fn charge(
        &self,
        id: &str,
        company: &str,
        operation: &str,
        amount: u128,
        now_ms: u64,
    ) -> Result<Charged, Vec<String>> {
        let mut held = self.hold();
        if let Some(c) = held.0.get(&charge_id(company, operation)) {
            match c.state {
                ChargeState::Confirmed => {
                    return Err(vec![format!(
                        "company {company}: operation {operation} was confirmed and charged already ({} {} to allowance {}) — agentd never confirms an operation twice",
                        c.amount, c.currency, c.allowance
                    )]);
                }
                ChargeState::Sent | ChargeState::Unknown => return Ok(Charged::Held),
                ChargeState::Refused | ChargeState::Credited => {}
            }
        }
        let a = self.allowance(id).map_err(|e| vec![e])?;
        let a = &a;
        let usage = self.usage(&held, a, now_ms, None);
        let above = judge(a, company, amount, usage, now_ms)?;
        let c = Charge {
            company: company.to_string(),
            operation: operation.to_string(),
            allowance: a.id.clone(),
            currency: a.currency.clone(),
            amount: format_amount(amount, SCALE),
            state: ChargeState::Sent,
            at_ms: now_ms,
            ended_ms: None,
        };
        self.put(&mut held, c).map_err(|e| vec![e])?;
        Ok(Charged::New(above))
    }

    /// The charge for `operation` at `company` once agentd's own record of
    /// the operation moved to the final `state` from a state that was not
    /// final (`confirmed`: the Company took a confirm of it): `succeeded`
    /// confirms it; `cancelled` credits it back; `failed` or `expired`
    /// keeps a taken one counted and releases one never taken. What it is
    /// now, when it moved.
    pub fn settle_final(
        &self,
        company: &str,
        operation: &str,
        state: &str,
        confirmed: bool,
    ) -> Option<ChargeState> {
        let c = self.charge_of(company, operation)?;
        let to = match state {
            "succeeded" => ChargeState::Confirmed,
            "cancelled" => ChargeState::Credited,
            _ if confirmed || c.state == ChargeState::Confirmed => ChargeState::Confirmed,
            _ => ChargeState::Refused,
        };
        if c.state == to || c.state == ChargeState::Credited || c.state == ChargeState::Refused {
            return None;
        }
        self.settle(company, operation, to);
        Some(to)
    }

    /// The charge for `operation` at `company` moved to `state`; nothing
    /// when there is none.
    pub fn settle(&self, company: &str, operation: &str, state: ChargeState) {
        let mut held = self.hold();
        let Some(mut c) = held.0.get(&charge_id(company, operation)).cloned() else {
            return;
        };
        if c.state == state || c.state == ChargeState::Credited {
            return;
        }
        c.state = state;
        c.ended_ms = Some(crate::state::now_ms());
        let _ = self.put(&mut held, c);
    }

    /// The charge for `operation` at `company`, if one is kept.
    pub fn charge_of(&self, company: &str, operation: &str) -> Option<Charge> {
        self.hold().0.get(&charge_id(company, operation)).cloned()
    }

    fn put(&self, held: &mut Charges, c: Charge) -> Result<(), String> {
        let id = format!("{PREFIX}{}", c.id());
        let state = serde_json::to_value(&c).map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Cred, &id, state, None)
            .map_err(|e| format!("the spending ledger could not keep the charge: {e}"))?;
        if self.durable.is_degraded() {
            return Err(
                "the spending ledger could not keep the charge: the store is degraded".into(),
            );
        }
        held.0.insert(c.id(), c);
        Ok(())
    }

    /// Forget the charges no allowance counts any more: its allowance is
    /// gone, or its period ended. How many.
    pub fn sweep(&self, allowances: &BTreeMap<String, Allowance>, now_ms: u64) -> usize {
        let mut held = self.hold();
        let old: Vec<String> = held
            .0
            .values()
            .filter(|c| {
                c.state != ChargeState::Sent
                    && allowances
                        .get(&c.allowance)
                        .is_none_or(|a| !a.holds(c.at_ms, now_ms))
            })
            .map(Charge::id)
            .collect();
        let mut n = 0;
        for id in old {
            if self
                .durable
                .delete(Kind::Cred, &format!("{PREFIX}{id}"))
                .is_ok()
            {
                held.0.remove(&id);
                n += 1;
            }
        }
        n
    }

    /// `payment.allowances` and the status document's `payments.allowances`:
    /// each allowance's current period, what was spent in it and how many
    /// payments were made.
    pub fn report(&self, allowances: &BTreeMap<String, Allowance>, now_ms: u64) -> Value {
        let held = self.hold();
        let list: Vec<Value> = allowances
            .values()
            .map(|a| {
                let u = self.usage(&held, a, now_ms, None);
                let start = period_start(a.per, now_ms);
                json!({
                    "id": a.id,
                    "title": a.title,
                    "currency": a.currency,
                    "period": a.per.as_str(),
                    "period_start": start.map(rfc3339),
                    "period_end": start.and_then(|s| period_end(a.per, s)).map(rfc3339),
                    "spent": format_amount(u.spent, SCALE),
                    "budget": format_amount(a.budget, SCALE),
                    "left": format_amount(a.budget.saturating_sub(u.spent), SCALE),
                    "max": format_amount(a.max, SCALE),
                    "approve_above": a.approve_above.map(|v| format_amount(v, SCALE)),
                    "payments": u.payments,
                    "times": a.times,
                })
            })
            .collect();
        json!({"allowances": list})
    }
}

/// The fields of an allowance that differ between two configurations:
/// what `payment.allowance.changed` names.
pub fn changed_fields(old: &cfg::Allowance, new: &cfg::Allowance) -> Vec<&'static str> {
    let mut out = Vec::new();
    macro_rules! field {
        ($($f:ident),*) => {$(
            if old.$f != new.$f {
                out.push(stringify!($f));
            }
        )*};
    }
    field!(
        title,
        currency,
        wallets,
        max,
        budget,
        per,
        times,
        payees,
        categories,
        from,
        until,
        approve_above,
        approver
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> u64 {
        crate::obs::log::rfc3339_ms(s).unwrap()
    }

    /// The calendar periods, in UTC, at their edges: a day from midnight, an
    /// ISO week from Monday, a month from the 1st (February in a leap year
    /// and not), a quarter from its first month, a year from January 1st —
    /// and `once` never renews.
    #[test]
    fn period_starts_are_calendar_periods_in_utc() {
        let start = |per, at: &str| period_start(per, ms(at)).map(rfc3339);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            start(Per::Day, "2026-11-03T23:59:59Z"),
            s("2026-11-03T00:00:00Z")
        );
        assert_eq!(
            start(Per::Day, "2026-11-04T00:00:00Z"),
            s("2026-11-04T00:00:00Z")
        );
        // 2026-11-02 is a Monday; Sunday the 8th is still its week.
        assert_eq!(
            start(Per::Week, "2026-11-08T23:59:59Z"),
            s("2026-11-02T00:00:00Z")
        );
        assert_eq!(
            start(Per::Week, "2026-11-09T00:00:00Z"),
            s("2026-11-09T00:00:00Z")
        );
        // An ISO week across a year's end: 2026-12-28 is a Monday.
        assert_eq!(
            start(Per::Week, "2027-01-01T12:00:00Z"),
            s("2026-12-28T00:00:00Z")
        );
        assert_eq!(
            start(Per::Month, "2024-02-29T23:00:00Z"),
            s("2024-02-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Month, "2024-03-01T00:00:00Z"),
            s("2024-03-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Month, "2026-12-31T23:59:59Z"),
            s("2026-12-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Quarter, "2026-06-30T23:59:59Z"),
            s("2026-04-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Quarter, "2026-07-01T00:00:00Z"),
            s("2026-07-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Quarter, "2026-02-14T00:00:00Z"),
            s("2026-01-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Year, "2024-12-31T23:59:59Z"),
            s("2024-01-01T00:00:00Z")
        );
        assert_eq!(
            start(Per::Year, "2025-01-01T00:00:00Z"),
            s("2025-01-01T00:00:00Z")
        );
        assert_eq!(start(Per::Once, "2026-11-03T00:00:00Z"), None);
        // Each period ends where the next starts.
        let end = |per, at: &str| period_end(per, ms(at)).map(rfc3339);
        assert_eq!(
            end(Per::Month, "2024-02-01T00:00:00Z"),
            s("2024-03-01T00:00:00Z")
        );
        assert_eq!(
            end(Per::Month, "2026-12-01T00:00:00Z"),
            s("2027-01-01T00:00:00Z")
        );
        assert_eq!(
            end(Per::Quarter, "2026-10-01T00:00:00Z"),
            s("2027-01-01T00:00:00Z")
        );
        assert_eq!(
            end(Per::Week, "2026-12-28T00:00:00Z"),
            s("2027-01-04T00:00:00Z")
        );
        assert_eq!(
            end(Per::Year, "2024-01-01T00:00:00Z"),
            s("2025-01-01T00:00:00Z")
        );
        assert_eq!(
            end(Per::Day, "2024-02-28T00:00:00Z"),
            s("2024-02-29T00:00:00Z")
        );
        // Every day of four years, a leap one among them: the start is at
        // or before the day, the end after it, and the end is the start of
        // the next period.
        for per in [Per::Day, Per::Week, Per::Month, Per::Quarter, Per::Year] {
            let mut t = ms("2023-01-01T12:00:00Z");
            while t < ms("2027-01-01T00:00:00Z") {
                let st = period_start(per, t).unwrap();
                let en = period_end(per, st).unwrap();
                assert!(st <= t && t < en, "{per:?} at {}", rfc3339(t));
                assert_eq!(period_start(per, en), Some(en), "{per:?}");
                t += DAY_MS;
            }
        }
    }

    /// A token amount converts exactly into the currency its asset names —
    /// one whole unit is one unit — and back, rounded down.
    #[test]
    fn amounts_convert_exactly() {
        // 20.5 USDC (6 decimals) is 20.5 USD.
        let usd = to_currency(20_500_000, 6).unwrap();
        assert_eq!(format_amount(usd, SCALE), "20.5");
        assert_eq!(from_currency(usd, 6), 20_500_000);
        // 1 atomic unit of a 6-decimal token.
        assert_eq!(format_amount(to_currency(1, 6).unwrap(), SCALE), "0.000001");
        // An 18-decimal token is held as it is.
        assert_eq!(to_currency(5, 18), Some(5));
        // Past the scale, no conversion.
        assert_eq!(to_currency(5, 19), None);
        // A cap in the currency below one atomic unit rounds down.
        assert_eq!(
            from_currency(parse_amount("0.0000015", SCALE).unwrap(), 6),
            1
        );
        assert_eq!(to_currency(u128::MAX, 6), None, "past 128 bits");
    }

    fn groceries() -> Allowance {
        Allowance::from_settings(
            "groceries",
            &cfg::Allowance {
                currency: "GBP".into(),
                max: Some("120".into()),
                budget: Some("400".into()),
                per: Some("month".into()),
                times: Some(3),
                payees: Some(vec!["tesco".into(), "ocado".into()]),
                approve_above: Some("60".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn gbp(s: &str) -> u128 {
        parse_amount(s, SCALE).unwrap()
    }

    /// Each limit an allowance sets, its reason naming the allowance and the
    /// limit; within them, whether the charge is above `approve_above`.
    #[test]
    fn a_charge_is_judged_against_every_limit() {
        let a = groceries();
        let now = ms("2026-11-03T10:00:00Z");
        let none = Usage::default();
        assert_eq!(judge(&a, "tesco", gbp("20"), none, now), Ok(false));
        assert_eq!(judge(&a, "tesco", gbp("60"), none, now), Ok(false));
        assert_eq!(judge(&a, "tesco", gbp("60.01"), none, now), Ok(true));
        let why = |r: Result<bool, Vec<String>>| r.unwrap_err().join("; ");
        let spent = Usage {
            spent: gbp("385"),
            payments: 2,
        };
        assert_eq!(
            why(judge(&a, "tesco", gbp("20"), spent, now)),
            "allowance groceries: budget 400 GBP per month, 385 spent, this payment 20"
        );
        assert!(
            judge(&a, "tesco", gbp("15"), spent, now).is_ok(),
            "at the budget is within it"
        );
        assert!(
            why(judge(&a, "tesco", gbp("130"), none, now))
                .contains("max 120 GBP, this payment 130")
        );
        let three = Usage {
            spent: gbp("30"),
            payments: 3,
        };
        assert!(
            why(judge(&a, "tesco", gbp("1"), three, now)).contains("times 3 per month, 3 made")
        );
        assert!(
            why(judge(&a, "aldi", gbp("1"), none, now))
                .contains("payee aldi is not one of its payees")
        );
        let mut cat = groceries();
        cat.categories = vec!["food".into()];
        assert!(
            why(judge(&cat, "tesco", gbp("1"), none, now))
                .contains("rail-cannot-enforce: categories")
        );
        let mut window = groceries();
        window.from_ms = Some(ms("2026-12-01T00:00:00Z"));
        assert!(
            why(judge(&window, "tesco", gbp("1"), none, now))
                .contains("from 2026-12-01T00:00:00Z, not yet")
        );
        window.from_ms = None;
        window.until_ms = Some(ms("2026-11-01T00:00:00Z"));
        assert!(
            why(judge(&window, "tesco", gbp("1"), none, now))
                .contains("until 2026-11-01T00:00:00Z, past")
        );
        // Every reason at once.
        let all = why(judge(&a, "aldi", gbp("500"), three, now));
        for want in ["payee", "max", "budget", "times"] {
            assert!(all.contains(want), "{want}: {all}");
        }
        let mut once = groceries();
        once.per = Per::Once;
        assert!(
            why(judge(&once, "tesco", gbp("20"), spent, now)).contains("budget 400 GBP in all")
        );
    }

    /// What the load refuses of an allowance, each naming the field.
    #[test]
    fn an_allowance_that_does_not_read_is_refused_by_field() {
        let base = cfg::Allowance {
            currency: "GBP".into(),
            max: Some("120".into()),
            budget: Some("400".into()),
            ..Default::default()
        };
        assert!(Allowance::from_settings("g", &base).is_ok());
        for (edit, says) in [
            (
                Box::new(|a: &mut cfg::Allowance| a.max = None) as Box<dyn Fn(&mut cfg::Allowance)>,
                "security.payments.allowances.g.max is not set",
            ),
            (
                Box::new(|a| a.budget = None),
                "allowances.g.budget is not set",
            ),
            (
                Box::new(|a| a.max = Some("1.5.0".into())),
                "allowances.g.max:",
            ),
            (
                Box::new(|a| a.per = Some("fortnight".into())),
                "allowances.g.per:",
            ),
            (
                Box::new(|a| a.from = Some("tomorrow".into())),
                "allowances.g.from:",
            ),
            (
                Box::new(|a| a.until = Some("2026-13-01".into())),
                "allowances.g.until:",
            ),
            (
                Box::new(|a| a.approve_above = Some("-1".into())),
                "allowances.g.approve_above:",
            ),
        ] {
            let mut a = base.clone();
            edit(&mut a);
            let e = Allowance::from_settings("g", &a).unwrap_err();
            assert!(e.contains(says), "{says}: {e}");
        }
    }

    /// Charges kept in the store: counted in their period, read back after
    /// a restart, not counted once the Company refused or the cancellation
    /// credited them, and swept once no period counts them.
    #[test]
    fn company_charges_count_until_credited() {
        use crate::store::Store;
        use crate::store::memory::MemoryStore;
        let store = Arc::new(MemoryStore::new());
        let durable = || {
            Arc::new(Durable::new(
                Arc::clone(&store) as Arc<dyn Store>,
                "t/",
                "t",
                crate::state::Policy::default(),
                None,
            ))
        };
        let mut a = groceries();
        a.payees = Some(vec!["shop".into()]);
        let now = crate::state::now_ms();
        let s = Spending::open(durable()).unwrap();
        s.set_allowance(a.clone());
        let g = "groceries";
        assert_eq!(
            s.charge(g, "shop", "op1", gbp("50"), now),
            Ok(Charged::New(false))
        );
        assert_eq!(
            s.charge(g, "shop", "op2", gbp("100"), now),
            Ok(Charged::New(true))
        );
        // Again for the same operation, its outcome not known yet: the
        // charge is kept as it is, counted once — never written over.
        assert_eq!(s.charge(g, "shop", "op2", gbp("1"), now), Ok(Charged::Held));
        assert_eq!(s.charge_of("shop", "op2").unwrap().amount, "100");
        a.max = gbp("300");
        s.set_allowance(a.clone());
        let e = s.charge(g, "shop", "op3", gbp("300"), now).unwrap_err();
        assert_eq!(
            e,
            ["allowance groceries: budget 400 GBP per month, 150 spent, this payment 300"]
        );
        assert!(
            s.charge_of("shop", "op3").is_none(),
            "a refused charge is not kept"
        );
        // A confirmed charge is never charged again: the same operation id
        // proposed again refuses.
        s.settle("shop", "op1", ChargeState::Confirmed);
        let e = s.charge(g, "shop", "op1", gbp("5"), now).unwrap_err();
        assert!(e[0].contains("was confirmed and charged already"), "{e:?}");
        assert!(s.confirmed_already("shop", "op1").is_some());
        assert_eq!(s.charge_of("shop", "op1").unwrap().amount, "50");
        // Judged against the allowance in force, not the one a caller read.
        let mut lower = a.clone();
        lower.max = gbp("10");
        s.set_allowance(lower);
        let e = s.charge(g, "shop", "op4", gbp("20"), now).unwrap_err();
        assert!(e[0].contains("max 10 GBP, this payment 20"), "{e:?}");
        s.set_allowance(a.clone());
        // agentd's record moving to a final state settles the charge: a
        // succeeded operation's stays counted whatever comes after.
        assert_eq!(
            s.settle_final("shop", "op1", "succeeded", true),
            None,
            "confirmed already"
        );
        assert_eq!(
            s.settle_final("shop", "op1", "cancelled", true),
            Some(ChargeState::Credited)
        );
        s.settle("shop", "op1", ChargeState::Confirmed);
        assert_eq!(
            s.charge_of("shop", "op1").unwrap().state,
            ChargeState::Credited
        );
        // A taken charge whose operation failed or expired stays counted.
        assert_eq!(
            s.settle_final("shop", "op2", "expired", true),
            Some(ChargeState::Confirmed)
        );
        let s = Spending::open(durable()).unwrap();
        let u = s.usage(&s.hold(), &a, now, None);
        assert_eq!(
            (u.spent, u.payments),
            (gbp("100"), 1),
            "op1 credited, op2 kept"
        );
        s.settle("shop", "op1", ChargeState::Refused);
        s.settle("shop", "op2", ChargeState::Credited);
        let u = s.usage(&s.hold(), &a, now, None);
        assert_eq!(u, Usage::default());
        // A credited charge stays credited.
        s.settle("shop", "op2", ChargeState::Confirmed);
        assert_eq!(
            s.charge_of("shop", "op2").unwrap().state,
            ChargeState::Credited
        );
        // Next month nothing counts, and the sweep forgets it.
        let later = period_end(Per::Month, a.since(now)).unwrap();
        let all = BTreeMap::from([(a.id.clone(), a.clone())]);
        assert_eq!(s.sweep(&all, later), 2);
        assert!(s.charge_of("shop", "op1").is_none());
    }
}
