//! 5-field UTC cron schedule — a standalone-mode convenience. RFC 0008. [feature: cron]
//!
//! Hand-rolled (no `croner` dep — the minimalism moat, rfcs/0002): a tiny parser
//! over `min hour dom month dow` plus a minute-stepping next-fire search, UTC
//! only. The recommended *production* schedule is still an external CronJob →
//! `agentd --mode once` (RFC 0008 §time-is-an-event-source, robust to clock skew
//! / missed ticks); this feature exists for a self-contained standalone agentd.

/// A parsed 5-field cron expression (UTC). Each field is a 64-bit set: bit `v`
/// is set when value `v` matches. `dom`/`dow` carry a `*`-vs-restricted flag for
/// the standard day-of-month / day-of-week OR semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: u64, // 0..=59
    hour: u64,   // 0..=23
    dom: u64,    // 1..=31
    month: u64,  // 1..=12
    dow: u64,    // 0..=6 (0 = Sunday)
    dom_restricted: bool,
    dow_restricted: bool,
}

impl CronExpr {
    /// Parse `min hour dom month dow`. Each field supports `*`, `*/step`, `a`,
    /// `a-b`, `a-b/step`, and comma lists thereof.
    pub fn parse(expr: &str) -> Result<CronExpr, String> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "cron needs exactly 5 fields (min hour dom month dow), got {}",
                f.len()
            ));
        }
        Ok(CronExpr {
            minute: parse_field(f[0], 0, 59)?,
            hour: parse_field(f[1], 0, 23)?,
            dom: parse_field(f[2], 1, 31)?,
            month: parse_field(f[3], 1, 12)?,
            dow: parse_field(f[4], 0, 6)?,
            dom_restricted: f[2] != "*",
            dow_restricted: f[4] != "*",
        })
    }

    /// The next unix second (UTC, on a minute boundary) strictly after `after`
    /// that matches. `None` if no match within ~4 years (covers Feb-29 exprs).
    pub fn next_after(&self, after: u64) -> Option<u64> {
        let mut t = (after / 60 + 1) * 60; // next whole minute after `after`
        for _ in 0..(366 * 24 * 60 * 4) {
            if self.matches(t) {
                return Some(t);
            }
            t += 60;
        }
        None
    }

    fn matches(&self, secs: u64) -> bool {
        let tm = decompose(secs);
        if !(bit(self.minute, tm.minute) && bit(self.hour, tm.hour) && bit(self.month, tm.month)) {
            return false;
        }
        // Day-of-month / day-of-week OR semantics: when both are restricted a
        // match on *either* fires; otherwise match whichever is restricted.
        let dom_ok = bit(self.dom, tm.dom);
        let dow_ok = bit(self.dow, tm.dow);
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }
}

fn bit(mask: u64, v: u64) -> bool {
    v < 64 && (mask & (1 << v)) != 0
}

/// Parse one field into a bitset over `[lo, hi]`.
fn parse_field(field: &str, lo: u64, hi: u64) -> Result<u64, String> {
    if field.is_empty() {
        return Err("empty cron field".into());
    }
    let mut mask = 0u64;
    for term in field.split(',') {
        let (range, step) = match term.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u64>()
                    .map_err(|_| format!("bad step in '{term}'"))?,
            ),
            None => (term, 1),
        };
        if step == 0 {
            return Err(format!("zero step in '{term}'"));
        }
        let (start, end) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            let s = a
                .parse::<u64>()
                .map_err(|_| format!("bad range '{range}'"))?;
            let e = b
                .parse::<u64>()
                .map_err(|_| format!("bad range '{range}'"))?;
            (s, e)
        } else {
            let v = range
                .parse::<u64>()
                .map_err(|_| format!("bad value '{range}'"))?;
            (v, v)
        };
        if start < lo || end > hi || start > end {
            return Err(format!("field value out of range [{lo}-{hi}]: '{term}'"));
        }
        let mut v = start;
        while v <= end {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok(mask)
}

struct Tm {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
}

/// Decompose a unix timestamp (UTC) into cron-relevant fields.
fn decompose(secs: u64) -> Tm {
    let days = secs / 86_400;
    let secs_of_day = secs % 86_400;
    let (_, month, dom) = crate::obs::log::civil_from_days(days as i64);
    Tm {
        minute: (secs_of_day % 3600) / 60,
        hour: secs_of_day / 3600,
        dom: dom as u64,
        month: month as u64,
        // 1970-01-01 was a Thursday → 4 with Sunday = 0.
        dow: (days + 4) % 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-01-01T00:00:00Z = 1704067200 (a Monday). A handy fixed anchor.
    const MON_2024_01_01: u64 = 1_704_067_200;

    #[test]
    fn decompose_known_instant() {
        let tm = decompose(MON_2024_01_01);
        assert_eq!(
            (tm.minute, tm.hour, tm.dom, tm.month, tm.dow),
            (0, 0, 1, 1, 1)
        ); // Monday
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(CronExpr::parse("* * * *").is_err()); // 4 fields
        assert!(CronExpr::parse("60 * * * *").is_err()); // minute out of range
        assert!(CronExpr::parse("* 24 * * *").is_err()); // hour out of range
        assert!(CronExpr::parse("*/0 * * * *").is_err()); // zero step
        assert!(CronExpr::parse("5-2 * * * *").is_err()); // inverted range
        assert!(CronExpr::parse("x * * * *").is_err()); // non-numeric
    }

    #[test]
    fn every_minute_fires_next_minute() {
        let c = CronExpr::parse("* * * * *").unwrap();
        assert_eq!(c.next_after(MON_2024_01_01), Some(MON_2024_01_01 + 60));
    }

    #[test]
    fn hourly_at_minute_30() {
        let c = CronExpr::parse("30 * * * *").unwrap();
        assert_eq!(c.next_after(MON_2024_01_01), Some(MON_2024_01_01 + 30 * 60));
    }

    #[test]
    fn weekday_nine_am_skips_weekend() {
        // 0 9 * * 1-5 — 2024-01-01 is Monday, so 09:00 the same day.
        let c = CronExpr::parse("0 9 * * 1-5").unwrap();
        assert_eq!(
            c.next_after(MON_2024_01_01),
            Some(MON_2024_01_01 + 9 * 3600)
        );
        // From Fri 2024-01-05 10:00 → next is Mon 2024-01-08 09:00.
        let fri_10 = MON_2024_01_01 + 4 * 86_400 + 10 * 3600;
        let mon_9 = MON_2024_01_01 + 7 * 86_400 + 9 * 3600;
        assert_eq!(c.next_after(fri_10), Some(mon_9));
    }

    #[test]
    fn step_and_list_fields() {
        let c = CronExpr::parse("0 */6 * * *").unwrap(); // 00:00, 06:00, 12:00, 18:00
        assert_eq!(
            c.next_after(MON_2024_01_01),
            Some(MON_2024_01_01 + 6 * 3600)
        );
        let c2 = CronExpr::parse("0 0 1,15 * *").unwrap(); // 1st and 15th at 00:00
        assert_eq!(
            c2.next_after(MON_2024_01_01),
            Some(MON_2024_01_01 + 14 * 86_400)
        );
    }
}
