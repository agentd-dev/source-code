// SPDX-License-Identifier: AGPL-3.0-only
//! Allowances, as the runtime holds them (RFC 0057): read from the live
//! settings at every payment, every Company charge and every answered
//! approval, so a reload that changes one applies at once; reported in the
//! status document and by `payment.allowances`; swept hourly; and each
//! change a reload makes said.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::reactor::Runtime;
use crate::spending::Allowance;
use crate::state::now_ms;

/// How often the spending ledger forgets what no period counts.
const SWEEP_EVERY_MS: u64 = 3600 * 1000;

impl Runtime {
    /// Every allowance as the settings say now.
    pub(crate) fn allowances(&self) -> BTreeMap<String, Allowance> {
        Allowance::all(&self.settings.security.payments)
    }

    /// The allowance `id` as the settings say now, or why there is none: a
    /// reload removed it.
    #[cfg_attr(not(any(feature = "x402", feature = "companies")), allow(dead_code))]
    pub(crate) fn allowance_now(&self, id: &str) -> Result<Allowance, String> {
        match self.settings.security.payments.allowances.get(id) {
            Some(a) => Allowance::from_settings(id, a),
            None => Err(format!(
                "allowance {id}: it is no longer a security.payments.allowances entry, so nothing is paid under it"
            )),
        }
    }

    /// `payment.allowances`, the op and the tool, and the status document's
    /// `payments.allowances`: each allowance's current period, its budget,
    /// what was spent in it and how many payments were made. Read-only.
    pub(crate) fn allowances_value(&self) -> Value {
        self.spending.report(&self.allowances(), now_ms())
    }

    /// Hourly: the spending ledger forgets the charges no allowance's
    /// period counts any more.
    pub(crate) fn spending_tick(&mut self) {
        let now = now_ms();
        if now.saturating_sub(self.spending_swept_ms) < SWEEP_EVERY_MS {
            return;
        }
        self.spending_swept_ms = now;
        let n = self.spending.sweep(&self.allowances(), now);
        if n > 0 {
            self.log
                .info("payment.allowance.swept", json!({"charges": n}));
        }
    }

    /// What a reload did to the allowances, said: each changed one, with
    /// the fields that changed, and each removed one. Whether any did.
    pub(crate) fn allowances_reloaded(&self, old: &crate::config::settings::Settings) -> bool {
        let (was, now) = (
            &old.security.payments.allowances,
            &self.settings.security.payments.allowances,
        );
        let mut moved = false;
        for (id, a) in now {
            match was.get(id) {
                Some(b) if b == a => {}
                Some(b) => {
                    moved = true;
                    self.log.info(
                        "payment.allowance.changed",
                        json!({"id": id, "fields": crate::spending::changed_fields(b, a)}),
                    );
                }
                None => {
                    moved = true;
                    self.log.info(
                        "payment.allowance.changed",
                        json!({"id": id, "added": true}),
                    );
                }
            }
        }
        for id in was.keys().filter(|id| !now.contains_key(*id)) {
            moved = true;
            self.log
                .info("payment.allowance.removed", json!({"id": id}));
        }
        moved
    }
}
