// SPDX-License-Identifier: AGPL-3.0-only
//! What was paid, and what may have been (RFC 0049 D4, D9): one record per
//! payment, in the store, written before the signed request leaves.
//!
//! A paid request is a run's step, named by its idempotency key; each
//! payment of it is an attempt, numbered from 1. A record is `sending` while
//! its request is out, then `settled`, `refused`, `unknown` or `unsent`.
//! Read at start, a record still `sending` — the process stopped with the
//! request out — becomes `unknown`: whatever it reached, it is never sent
//! again. A request with a `settled` or `unknown` record is never paid
//! again; one whose every attempt was `refused` or `unsent` may be, by a
//! new attempt with a new nonce.
//!
//! The caps count every payment that left agentd or may have (`sending`,
//! `unknown`, `settled`), and a `refused` one until its `validBefore`: its
//! authorization is still signed and valid, held by a server that said it
//! did not settle.
//!
//! Records are `cred` entries under `payment/ledger/<request>.<attempt>`,
//! plain — the nonce, the amount and the payee are no secret — and kept
//! while their run is live and 25 hours at least, which is longer than any
//! cap looks back.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::policy::Spent;
use super::scheme::Account;
use crate::state::{Durable, Kind};

/// Where the records are kept, in the store.
pub const PREFIX: &str = "payment/ledger/";
/// How long a record is kept at least, once its run is no longer live.
pub const KEEP_MS: u64 = 25 * 3600 * 1000;
/// The window `per_day` and `per_payee` look back over.
pub const DAY_MS: u64 = 24 * 3600 * 1000;

/// Where a payment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The signed request is out, or about to be.
    Sending,
    /// The server said it settled.
    Settled,
    /// The server said it did not settle.
    Refused,
    /// What landed is not known: final.
    Unknown,
    /// Nothing left agentd: the signature was never made, or the connection
    /// never opened.
    Unsent,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Sending => "sending",
            State::Settled => "settled",
            State::Refused => "refused",
            State::Unknown => "unknown",
            State::Unsent => "unsent",
        }
    }
}

/// One payment's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The request: the step's idempotency key.
    pub request: String,
    /// Which payment of the request, from 1.
    pub attempt: u32,
    pub run: String,
    pub step: String,
    pub workflow: String,
    pub wallet: String,
    pub network: String,
    /// The asset's name, and its CAIP-19 id.
    pub asset: String,
    pub asset_id: String,
    pub payee: String,
    /// The payee's address, as its family writes it.
    pub pay_to: String,
    /// Atomic units, decimal.
    pub amount: String,
    pub nonce: String,
    pub valid_before: u64,
    /// The URL paid for, its query left out.
    pub url: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    /// When it was reserved, epoch milliseconds.
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
}

impl Record {
    /// Where it is kept: its request and attempt.
    pub fn id(&self) -> String {
        format!("{}.{}", self.request, self.attempt)
    }

    /// The atomic amount.
    pub fn atomic(&self) -> u128 {
        self.amount.parse().unwrap_or(u128::MAX)
    }

    /// Whether it counts against the caps at `now_ms`.
    pub fn counts(&self, now_ms: u64) -> bool {
        match self.state {
            State::Sending | State::Settled | State::Unknown => true,
            State::Refused => self.valid_before.saturating_mul(1000) > now_ms,
            State::Unsent => false,
        }
    }

    /// Whether its request is never paid again: it was paid, or may have
    /// been.
    pub fn final_(&self) -> bool {
        matches!(self.state, State::Sending | State::Settled | State::Unknown)
    }
}

/// The ledger: every record kept, in memory by id, written through to the
/// store.
pub struct Ledger {
    durable: Arc<Durable>,
    records: BTreeMap<String, Record>,
}

impl Ledger {
    /// Read every record; a `sending` one becomes `unknown`, written back.
    /// `Ok((ledger, the requests made unknown))`.
    pub fn open(durable: Arc<Durable>) -> Result<(Ledger, Vec<Record>), String> {
        let list = durable
            .list_prefix(Kind::Cred, PREFIX)
            .map_err(|e| format!("the payment ledger: {e}"))?;
        let mut records = BTreeMap::new();
        for ks in list {
            let Some((_, id)) =
                crate::store::parse_key(durable.prefix(), durable.instance(), &ks.key)
            else {
                continue;
            };
            if !id.starts_with(PREFIX) {
                continue;
            }
            let env = durable
                .get(Kind::Cred, id)
                .map_err(|e| format!("the payment ledger: {id}: {e}"))?;
            let Some(env) = env else { continue };
            let rec: Record = serde_json::from_value(env.state).map_err(|e| {
                format!("the payment ledger: {id} is not a payment record ({e}); agentd will not pay while it cannot read what it paid")
            })?;
            records.insert(rec.id(), rec);
        }
        let mut ledger = Ledger { durable, records };
        let mut made_unknown = Vec::new();
        let sending: Vec<Record> = ledger
            .records
            .values()
            .filter(|r| r.state == State::Sending)
            .cloned()
            .collect();
        for mut r in sending {
            r.state = State::Unknown;
            r.error_reason = Some("agentd stopped while the payment was out".into());
            r.ended_ms = Some(crate::state::now_ms());
            ledger.put(r.clone())?;
            made_unknown.push(r);
        }
        Ok((ledger, made_unknown))
    }

    /// Attempt `attempt` of `request`.
    pub fn get(&self, request: &str, attempt: u32) -> Option<&Record> {
        self.records.get(&format!("{request}.{attempt}"))
    }

    /// The record that keeps `request` from being paid again, if one does:
    /// an attempt that was paid, or may have been.
    pub fn blocking(&self, request: &str) -> Option<&Record> {
        self.records
            .values()
            .filter(|r| r.request == request && r.final_())
            .max_by_key(|r| r.attempt)
    }

    /// The number the next attempt of `request` takes.
    pub fn next_attempt(&self, request: &str) -> u32 {
        self.records
            .values()
            .filter(|r| r.request == request)
            .map(|r| r.attempt)
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Write `rec`: in the store first, which must keep it — a store that
    /// failed or only said it degraded refuses — then here.
    pub fn put(&mut self, rec: Record) -> Result<(), String> {
        let id = format!("{PREFIX}{}", rec.id());
        let state = serde_json::to_value(&rec).map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Cred, &id, state, None)
            .map_err(|e| format!("the payment ledger could not keep the record: {e}"))?;
        if self.durable.is_degraded() {
            return Err(
                "the payment ledger could not keep the record: the store is degraded".into(),
            );
        }
        self.records.insert(rec.id(), rec);
        Ok(())
    }

    /// What counts against `asset`'s caps for `run` and `pay_to` at `now_ms`.
    pub fn spent(&self, asset: &str, run: &str, pay_to: &Account, now_ms: u64) -> Spent {
        let mut s = Spent::default();
        let since = now_ms.saturating_sub(DAY_MS);
        let payee = pay_to.to_string();
        for r in self
            .records
            .values()
            .filter(|r| r.asset == asset && r.counts(now_ms))
        {
            let a = r.atomic();
            if r.run == run {
                s.run = s.run.saturating_add(a);
            }
            if r.at_ms >= since {
                s.day = s.day.saturating_add(a);
                if r.pay_to == payee {
                    s.payee = s.payee.saturating_add(a);
                }
            }
        }
        s
    }

    /// Forget records older than [`KEEP_MS`] whose run is not live: past
    /// every window a cap looks back over, and past every `validBefore`;
    /// how many.
    pub fn sweep(&mut self, now_ms: u64, live: &dyn Fn(&str) -> bool) -> usize {
        let old: Vec<String> = self
            .records
            .values()
            .filter(|r| {
                r.at_ms.saturating_add(KEEP_MS) < now_ms
                    && r.state != State::Sending
                    && !live(&r.run)
            })
            .map(Record::id)
            .collect();
        let mut n = 0;
        for id in old {
            if self
                .durable
                .delete(Kind::Cred, &format!("{PREFIX}{id}"))
                .is_ok()
            {
                self.records.remove(&id);
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::Store;
    use crate::store::memory::MemoryStore;

    /// A fresh `Durable` over `store`: what a restart opens.
    fn durable(store: &Arc<MemoryStore>) -> Arc<Durable> {
        Arc::new(Durable::new(
            Arc::clone(store) as Arc<dyn Store>,
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        ))
    }

    pub fn record(request: &str, run: &str, amount: u128, state: State, at_ms: u64) -> Record {
        Record {
            request: request.into(),
            attempt: 1,
            run: run.into(),
            step: "pay".into(),
            workflow: "w".into(),
            wallet: "ops".into(),
            network: "eip155:8453".into(),
            asset: "usdc-base".into(),
            asset_id: "eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
            payee: "weather".into(),
            pay_to: "0x209693Bc6afc0C5328bA36FaF03C514EF312287C".into(),
            amount: amount.to_string(),
            nonce: format!("0x{}", "11".repeat(32)),
            valid_before: at_ms / 1000 + 60,
            url: "https://api.example/data".into(),
            state,
            transaction: None,
            payer: None,
            error_reason: None,
            approved_by: None,
            at_ms,
            ended_ms: None,
        }
    }

    /// A record kept is read back after a reopen; one still `sending` when
    /// the process stopped is `unknown` then, and stays so — final.
    #[test]
    fn a_payment_out_when_agentd_stopped_is_unknown_after_it() {
        let store = Arc::new(MemoryStore::new());
        let now = crate::state::now_ms();
        {
            let (mut l, made) = Ledger::open(durable(&store)).unwrap();
            assert!(made.is_empty());
            l.put(record("r1", "run1", 1000, State::Settled, now))
                .unwrap();
            l.put(record("r2", "run1", 2000, State::Sending, now))
                .unwrap();
        }
        let (l, made) = Ledger::open(durable(&store)).unwrap();
        assert_eq!(made.len(), 1);
        assert_eq!(made[0].request, "r2");
        assert_eq!(l.get("r2", 1).unwrap().state, State::Unknown);
        assert_eq!(l.blocking("r2").unwrap().state, State::Unknown);
        assert_eq!(l.get("r1", 1).unwrap().state, State::Settled);
        assert!(
            l.blocking("r1").is_some(),
            "a settled request is never paid again"
        );
        let (l, made) = Ledger::open(durable(&store)).unwrap();
        assert!(made.is_empty(), "unknown stays unknown");
        assert_eq!(l.get("r2", 1).unwrap().state, State::Unknown);
    }

    /// A request whose payment was refused or never sent may be paid again,
    /// as a new attempt beside the old one — whose refused authorization
    /// still counts against the caps until its validBefore; once an attempt
    /// may have landed, none follows.
    #[test]
    fn a_refused_payment_may_be_made_again_as_a_new_attempt() {
        let store = Arc::new(MemoryStore::new());
        let now = crate::state::now_ms();
        let (mut l, _) = Ledger::open(durable(&store)).unwrap();
        assert_eq!(l.next_attempt("r"), 1);
        l.put(record("r", "run1", 100, State::Refused, now))
            .unwrap();
        assert!(l.blocking("r").is_none());
        assert_eq!(l.next_attempt("r"), 2);
        let mut unsent = record("r", "run1", 100, State::Unsent, now);
        unsent.attempt = 2;
        l.put(unsent).unwrap();
        assert!(l.blocking("r").is_none());
        let mut third = record("r", "run1", 100, State::Sending, now);
        third.attempt = l.next_attempt("r");
        assert_eq!(third.attempt, 3);
        l.put(third).unwrap();
        let (l, _) = Ledger::open(durable(&store)).unwrap();
        assert_eq!(
            l.get("r", 1).unwrap().state,
            State::Refused,
            "kept beside the others"
        );
        assert_eq!(l.blocking("r").unwrap().attempt, 3);
        let payee = Account::parse("0x209693Bc6afc0C5328bA36FaF03C514EF312287C").unwrap();
        assert_eq!(
            l.spent("usdc-base", "run1", &payee, now).run,
            200,
            "the refused one and the third"
        );
    }

    /// What counts against the caps: by run, by day, by payee; a refused
    /// payment until its validBefore; an unsent one never; another asset
    /// never.
    #[test]
    fn the_caps_count_what_left_or_may_have() {
        let store = Arc::new(MemoryStore::new());
        let now = crate::state::now_ms();
        let (mut l, _) = Ledger::open(durable(&store)).unwrap();
        l.put(record("a", "run1", 100, State::Settled, now))
            .unwrap();
        l.put(record("b", "run1", 200, State::Unknown, now))
            .unwrap();
        l.put(record("c", "run2", 400, State::Sending, now))
            .unwrap();
        l.put(record("d", "run1", 800, State::Unsent, now)).unwrap();
        l.put(record("e", "run2", 1600, State::Refused, now))
            .unwrap();
        let mut expired = record("f", "run2", 3200, State::Refused, now);
        expired.valid_before = now / 1000 - 1;
        l.put(expired).unwrap();
        l.put(record("g", "run3", 6400, State::Settled, now - DAY_MS - 1))
            .unwrap();
        let mut other = record("h", "run1", 12800, State::Settled, now);
        other.asset = "eurc".into();
        l.put(other).unwrap();
        let payee = Account::parse("0x209693Bc6afc0C5328bA36FaF03C514EF312287C").unwrap();
        let s = l.spent("usdc-base", "run1", &payee, now);
        assert_eq!(s.run, 300);
        assert_eq!(s.day, 100 + 200 + 400 + 1600);
        assert_eq!(s.payee, s.day);
        let elsewhere = Account::parse("0x857b06519E91e3A54538791bDbb0E22373e36b66").unwrap();
        assert_eq!(l.spent("usdc-base", "run3", &elsewhere, now).payee, 0);
        assert_eq!(
            l.spent("usdc-base", "run3", &elsewhere, now).run,
            6400,
            "a run's own counts past the day"
        );
    }
}
