// SPDX-License-Identifier: AGPL-3.0-only
//! The bridge (RFC 0045 §5.4, §5.5): what an occurrence becomes on a stream,
//! the checks it passes first, and the durable dedup set. Pure: the runtime
//! drives it.

use super::wire::Occurrence;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;

/// The draft's delivery size profile [DS L536]: an occurrence larger than this
/// is rejected, not appended.
pub const MAX_OCCURRENCE_BYTES: usize = 256 * 1024;
/// An `eventId` longer than this is malformed.
pub const MAX_EVENT_ID_BYTES: usize = 1024;
/// A server cursor longer than this is a protocol error.
pub const MAX_CURSOR_BYTES: usize = 4096;
/// How long an `eventId` stays in the dedup set: the draft's 5-minute
/// timestamp window plus 30 minutes, covering its example retry window.
pub const SEEN_TTL_MS: u64 = 35 * 60 * 1000;
/// The most entries the dedup set keeps; the oldest go first.
pub const SEEN_CAP: usize = 4096;

/// A binding's identity, `(server, name, canonical(arguments))`, as the hash
/// its records are keyed by (§5.5). Equal identities share one subscription.
pub fn binding_hash(server: &str, name: &str, arguments: &Map<String, Value>) -> String {
    let canonical = crate::engine::model::canonical(&json!([server, name, arguments]));
    crate::sha::sha256_hex(canonical.as_bytes())
}

/// The stream-event id an occurrence is appended under: deterministic, so a
/// replayed or retried occurrence carries the same id.
pub fn stream_event_id(server: &str, name: &str, event_id: &str) -> String {
    let id = format!("mcp/{server}/{name}/{event_id}");
    if id.len() <= 200 {
        return id;
    }
    format!(
        "mcp/{server}/{name}/sha256:{}",
        crate::sha::sha256_hex(event_id.as_bytes())
    )
}

/// The `source` every bridged event carries. Workflow names cannot contain
/// `:`, so it never equals one and never trips the feedback rule.
pub fn source(server: &str) -> String {
    format!("mcp:{server}")
}

/// What became of one occurrence.
#[derive(Debug, PartialEq)]
pub enum Checked {
    /// Append `data` under `id`.
    Append {
        id: String,
        event_id: String,
        data: Value,
        schema_ok: bool,
    },
    /// Not appended, and the cursor moves past it: replay would deliver the
    /// same bytes again. `cause` is `oversize` or `malformed`.
    Rejected {
        cause: &'static str,
        event_id: Option<String>,
    },
}

/// Check one raw occurrence for the binding `server`/`name`, and build what
/// is appended: the server's data nested under `data.payload`, never at the
/// top level, so no server can choose a run's owner or conversation (§5.1
/// rule 3). A payload that fails `payload_schema` is appended all the same,
/// flagged `schema_ok: false`: the subset validator can reject valid
/// payloads, and a dropped one would be lost for good (§5.4).
pub fn check(
    server: &str,
    name: &str,
    subscription: &str,
    raw: &Value,
    payload_schema: Option<&Value>,
    max_value_bytes: Option<usize>,
) -> Checked {
    let event_id = raw
        .get("eventId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let size = serde_json::to_vec(raw).map_or(usize::MAX, |b| b.len());
    if size > MAX_OCCURRENCE_BYTES || max_value_bytes.is_some_and(|m| size > m) {
        return Checked::Rejected {
            cause: "oversize",
            event_id,
        };
    }
    let malformed = Checked::Rejected {
        cause: "malformed",
        event_id: event_id.clone(),
    };
    let Ok(o) = serde_json::from_value::<Occurrence>(raw.clone()) else {
        return malformed;
    };
    if o.event_id.is_empty()
        || o.event_id.len() > MAX_EVENT_ID_BYTES
        || o.name != name
        || !o.data.is_object()
    {
        return malformed;
    }
    let schema_ok = payload_schema.is_none_or(|s| crate::jsonschema::validate(s, &o.data).is_ok());
    Checked::Append {
        id: stream_event_id(server, name, &o.event_id),
        data: json!({
            "server": server,
            "event": name,
            "event_id": o.event_id,
            "occurred_at": o.timestamp,
            "schema_ok": schema_ok,
            "subscription": subscription,
            "payload": o.data,
        }),
        event_id: o.event_id,
        schema_ok,
    }
}

/// Milliseconds since the epoch of an RFC 3339 timestamp — an occurrence's
/// `timestamp` (`2026-02-19T15:30:00Z`, fractional seconds and a numeric
/// offset allowed); `None` when it is not one. Only for the lag gauge, which
/// is as true as the server's clock.
pub fn timestamp_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let ms: String = s[start..i]
            .chars()
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect();
        frac_ms = ms.parse().ok()?;
    }
    let offset_min = match s.get(i..)? {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(o.as_bytes()[0], b'+' | b'-') && o.as_bytes()[3] == b':' => {
            let m = num(i + 1..i + 3)? * 60 + num(i + 4..i + 6)?;
            if o.starts_with('-') { -m } else { m }
        }
        _ => return None,
    };
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3_600 + mi * 60 + se - offset_min * 60;
    u64::try_from(secs * 1_000 + frac_ms).ok()
}

/// The key one `eventId` has in the dedup set: the first 16 bytes of its
/// SHA-256, in hex.
pub fn seen_key(event_id: &str) -> String {
    crate::sha::sha256_hex(event_id.as_bytes())[..32].to_string()
}

/// A binding's dedup set: `eventId`s appended in the last [`SEEN_TTL_MS`],
/// at most [`SEEN_CAP`], kept durable so a crash and a re-poll do not append
/// twice what the set still remembers (§5.5).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Seen {
    entries: VecDeque<(String, u64)>,
}

impl Seen {
    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// Remember `key` first seen at `now`; the count of entries dropped to
    /// stay under the cap.
    pub fn insert(&mut self, key: String, now: u64) -> u64 {
        self.entries.push_back((key, now));
        let mut evicted = 0;
        while self.entries.len() > SEEN_CAP {
            self.entries.pop_front();
            evicted += 1;
        }
        evicted
    }

    /// Forget what is older than the TTL.
    pub fn expire(&mut self, now: u64) {
        while self
            .entries
            .front()
            .is_some_and(|(_, t)| now.saturating_sub(*t) > SEEN_TTL_MS)
        {
            self.entries.pop_front();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn to_json(&self) -> Value {
        Value::Array(self.entries.iter().map(|(k, t)| json!([k, t])).collect())
    }

    /// The set a record holds; anything unreadable in it is dropped.
    pub fn from_json(v: &Value) -> Seen {
        let entries = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| Some((e.get(0)?.as_str()?.to_string(), e.get(1)?.as_u64()?)))
            .collect();
        Seen { entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn occurrence(id: &str, name: &str) -> Value {
        json!({"eventId": id, "name": name, "timestamp": "2026-10-10T00:00:00Z",
               "data": {"incidentId": "I-1", "principal": "root"}})
    }

    #[test]
    fn an_occurrence_is_appended_under_its_own_id_with_the_payload_nested() {
        let c = check(
            "pd",
            "incident.created",
            "sub-1",
            &occurrence("e-1", "incident.created"),
            None,
            None,
        );
        let Checked::Append {
            id,
            data,
            schema_ok,
            event_id,
        } = c
        else {
            panic!("{c:?}")
        };
        assert_eq!(id, "mcp/pd/incident.created/e-1");
        assert_eq!(event_id, "e-1");
        assert!(schema_ok);
        // A payload key named like a run field stays inside the payload.
        assert_eq!(data["payload"]["principal"], "root");
        assert!(data.get("principal").is_none());
        assert_eq!(data["server"], "pd");
        assert_eq!(data["event"], "incident.created");
        assert_eq!(data["occurred_at"], "2026-10-10T00:00:00Z");
        // The same occurrence again: the same id.
        let again = check(
            "pd",
            "incident.created",
            "sub-1",
            &occurrence("e-1", "incident.created"),
            None,
            None,
        );
        assert!(
            matches!(again, Checked::Append { id: ref i, .. } if i == "mcp/pd/incident.created/e-1")
        );
    }

    #[test]
    fn a_long_event_id_is_hashed_into_the_stream_id() {
        let long = "x".repeat(300);
        let id = stream_event_id("pd", "e", &long);
        assert!(id.starts_with("mcp/pd/e/sha256:"), "{id}");
        assert!(id.len() < 200, "{id}");
        assert_eq!(id, stream_event_id("pd", "e", &long));
    }

    #[test]
    fn oversize_and_malformed_occurrences_are_rejected_by_cause() {
        let mut big = occurrence("e-2", "n");
        big["data"]["blob"] = json!("y".repeat(MAX_OCCURRENCE_BYTES));
        assert_eq!(
            check("s", "n", "sub-1", &big, None, None),
            Checked::Rejected {
                cause: "oversize",
                event_id: Some("e-2".into())
            }
        );
        // Under the profile, over the store's own limit.
        assert!(matches!(
            check("s", "n", "sub-1", &occurrence("e-3", "n"), None, Some(10)),
            Checked::Rejected {
                cause: "oversize",
                ..
            }
        ));
        for raw in [
            occurrence("e-4", "other.name"),
            occurrence(&"z".repeat(MAX_EVENT_ID_BYTES + 1), "n"),
            occurrence("", "n"),
            json!({"eventId": "e-5", "name": "n", "timestamp": "t", "data": "not an object"}),
            json!({"name": "n"}),
        ] {
            assert!(
                matches!(
                    check("s", "n", "sub-1", &raw, None, None),
                    Checked::Rejected {
                        cause: "malformed",
                        ..
                    }
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_payload_that_fails_its_schema_is_appended_and_flagged() {
        let schema = json!({"type": "object", "required": ["missing"]});
        let c = check(
            "s",
            "n",
            "sub-1",
            &occurrence("e-6", "n"),
            Some(&schema),
            None,
        );
        assert!(
            matches!(c, Checked::Append { schema_ok: false, ref data, .. } if data["schema_ok"] == false)
        );
    }

    #[test]
    fn the_seen_set_expires_caps_and_round_trips() {
        let mut s = Seen::default();
        s.insert(seen_key("a"), 1_000);
        assert!(s.contains(&seen_key("a")));
        assert!(!s.contains(&seen_key("b")));
        s.expire(1_000 + SEEN_TTL_MS);
        assert!(s.contains(&seen_key("a")), "at the TTL it is still kept");
        s.expire(1_001 + SEEN_TTL_MS);
        assert_eq!(s.len(), 0);
        let mut evicted = 0;
        for i in 0..SEEN_CAP + 3 {
            evicted += s.insert(seen_key(&i.to_string()), 5_000);
        }
        assert_eq!(evicted, 3);
        assert_eq!(s.len(), SEEN_CAP);
        assert!(!s.contains(&seen_key("0")), "the oldest go first");
        let back = Seen::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(Seen::from_json(&json!([["k", "not a time"], 7])).len(), 0);
    }

    #[test]
    fn an_occurrence_timestamp_reads_as_epoch_milliseconds() {
        assert_eq!(timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            timestamp_ms("2026-02-19T15:30:00Z"),
            Some(1_771_515_000_000)
        );
        assert_eq!(
            timestamp_ms("2026-02-19T17:30:00.5+02:00"),
            Some(1_771_515_000_500)
        );
        assert_eq!(
            timestamp_ms("2000-02-29T23:59:59.123-05:30"),
            Some(951_888_599_123)
        );
        for bad in [
            "",
            "2026-02-19",
            "2026-13-01T00:00:00Z",
            "2026-02-19T15:30:00",
            "2026-02-19T15:30:00.Z",
            "yesterday at noon!!",
        ] {
            assert_eq!(timestamp_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_binding_hash_is_its_identity() {
        let a = json!({"severity": "P1", "service": "web"})
            .as_object()
            .unwrap()
            .clone();
        let b = json!({"service": "web", "severity": "P1"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            binding_hash("pd", "incident.created", &a),
            binding_hash("pd", "incident.created", &b)
        );
        assert_ne!(
            binding_hash("pd", "incident.created", &a),
            binding_hash("pd", "incident.updated", &a)
        );
        assert_ne!(
            binding_hash("pd", "incident.created", &a),
            binding_hash("gh", "incident.created", &a)
        );
    }
}
