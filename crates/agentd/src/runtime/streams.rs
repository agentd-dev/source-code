// SPDX-License-Identifier: AGPL-3.0-only
//! **Event streams**: named, durable, append-only
//! sequences workflows publish to (`emit`) and consume from (`stream`
//! starts) — including each other's, which is the point: streams are
//! instance-shared, so one workflow's `emit` is another's trigger.
//!
//! The property no other edge has: a **durable consumer offset**.
//! `subscribe` collapses history to the latest value, `webhook` and `signal`
//! drop what nobody was ready for; a `stream` consumer that was down for an
//! hour processes the hour's events after restart, in order. Delivery is
//! at-least-once with dedup by event id on the consumer side — the id of an
//! emitted event is the emitting step's derived idempotency key, so a
//! crash-replayed `emit` appends a second copy under the SAME id and the
//! consumer's recent-id ring drops it.
//!
//! Storage: `Kind::Event`, keyed `<stream>/e<seq:020>`; per-stream head/tail
//! counters in the manifest (`streams`), retention enforced at append
//! (count-based eagerly, age-based amortized). Appends are admissions: the
//! pressure system gates them like every other way of creating durable work.

use crate::state::{Kind, now_ms};
use serde_json::{Value, json};

/// How many events one consumer advances per reactor pass — bounds the time
/// the single-writer loop spends per tick on one busy stream while still
/// draining a backlog quickly (32/pass ≈ thousands/sec).
const BATCH: usize = 32;
/// Recent event ids each consumer remembers for at-least-once dedup.
const DEDUP_RING: usize = 64;

/// Subject match: exact, or a `prefix.*` glob (one trailing star).
pub fn subject_matches(pattern: &str, subject: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => subject.starts_with(prefix),
        None => pattern == subject,
    }
}

fn key(stream: &str, seq: u64) -> String {
    format!("{stream}/e{seq:020}")
}

impl super::reactor::Runtime {
    /// Append one event. Fail-closed on an undeclared stream; sheds under
    /// pressure like every admission.
    pub(crate) fn append_event(
        &mut self,
        stream: &str,
        subject: &str,
        correlation: Option<&str>,
        data: Value,
        id: &str,
        source: &str,
    ) -> Result<u64, String> {
        let Some(cfg) = self.settings.streams.get(stream).cloned() else {
            return Err(format!(
                "stream {stream:?} is not declared (add it under `streams:`)"
            ));
        };
        if let Some(cause) = self.pressure.refusal(false) {
            return Err(format!("emit refused: {cause}"));
        }
        let mut meta = self
            .durable
            .manifest()
            .streams
            .get(stream)
            .copied()
            .unwrap_or_default();
        if meta.first == 0 {
            meta.first = 1;
        }
        meta.seq += 1;
        let seq = meta.seq;
        let event = json!({
            "id": id, "stream": stream, "subject": subject, "seq": seq,
            "ts": now_ms(), "source": source,
            "correlation": correlation, "data": data,
        });
        self.durable
            .put(Kind::Event, &key(stream, seq), event, None)
            .map_err(|e| format!("stream {stream:?} append: {e}"))?;
        // Retention. Count-based is exact; age-based is amortized (a few head
        // reads per append) — either way a trim is an EVENT-adjacent fact
        // worth one log line, never silence.
        let mut trimmed = 0u64;
        while seq - meta.first + 1 > cfg.max_events() {
            let _ = self.durable.delete(Kind::Event, &key(stream, meta.first));
            meta.first += 1;
            trimmed += 1;
        }
        if let Some(max_age) = cfg.max_age_ms() {
            let cutoff = now_ms().saturating_sub(max_age);
            let mut budget = 8;
            while budget > 0 && meta.first < seq {
                let old = self
                    .durable
                    .get(Kind::Event, &key(stream, meta.first))
                    .ok()
                    .flatten();
                let expired = old
                    .as_ref()
                    .and_then(|e| e.state.get("ts"))
                    .and_then(Value::as_u64)
                    .is_some_and(|ts| ts < cutoff);
                if !expired {
                    break;
                }
                let _ = self.durable.delete(Kind::Event, &key(stream, meta.first));
                meta.first += 1;
                trimmed += 1;
                budget -= 1;
            }
        }
        if trimmed > 0 {
            self.log.info(
                "stream.trimmed",
                json!({"stream": stream, "events": trimmed, "first": meta.first}),
            );
        }
        self.durable.manifest_update(|m| {
            m.streams.insert(stream.to_string(), meta);
        });
        // Wake same-iteration consumers: without this a same-process
        // produce->consume pipeline advances at tick cadence (up to 200 ms
        // per hop) instead of engine speed.
        self.stream_dirty = true;
        Ok(seq)
    }

    /// Append everything the runtime-events tap queued since the last tick.
    ///
    /// The tap runs on the logging hot path with no access to the state owner,
    /// so it queues and this drains. That indirection is also what puts the
    /// daemon's own telemetry behind the same pressure gate as every other
    /// admission: under shedding these appends are refused like anything else,
    /// which is the only reason a `pressure.shed` storm cannot amplify itself
    /// into writes on the disk that caused it.
    ///
    /// A refused or failed append is dropped, never retried. Telemetry that
    /// queues behind a full disk is a second outage, not a record.
    pub(crate) fn drain_runtime_events(&mut self) {
        let (events, dropped) = crate::obs::log::drain_runtime_tap();
        if dropped > 0 {
            // Say so rather than losing it silently: a gap the consumer cannot
            // see is worse than no stream at all.
            self.log
                .warn("stream.tap.dropped", json!({"events": dropped}));
        }
        if events.is_empty() {
            return;
        }
        // Everything below logs, and those logs must not become more events to
        // append — see `tap_drain_guard`.
        let _guard = crate::obs::log::tap_drain_guard();
        let mut appended = 0usize;
        let mut refused = 0usize;
        for ev in events {
            if ev.stream.is_empty() {
                continue;
            }
            // `source` must name whoever CAUSED the event, not the tap, or the
            // self-trigger rule cannot see a workflow watching its own
            // completions — and `run.done` is exactly the event a watcher
            // wants, so it would re-fire on its own run forever. Runtime
            // events about a workflow already carry its name; everything else
            // is attributed to the daemon.
            let source = ev
                .data
                .get("workflow")
                .and_then(Value::as_str)
                .unwrap_or("_runtime")
                .to_string();
            let id = crate::state::ulid::new();
            match self.append_event(&ev.stream, &ev.subject, None, ev.data, &id, &source) {
                Ok(_) => appended += 1,
                Err(_) => refused += 1,
            }
        }
        if refused > 0 {
            self.log.warn(
                "stream.tap.refused",
                json!({"events": refused, "appended": appended}),
            );
        }
    }

    /// Advance every armed `stream` consumer: walk `offset+1..=seq`, fire a
    /// run per matching event, one durable start-state write per batch.
    /// Consumers hear EVERY producer on the stream — other workflows
    /// included; the self-trigger rule is by event `source`, not by stream.
    pub(crate) fn poll_stream_starts(&mut self) {
        let consumers: Vec<(String, String, serde_json::Map<String, Value>)> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| s.kind == "stream")
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, spec) in consumers {
            let Some(stream) = spec.get("stream").and_then(Value::as_str) else {
                continue;
            };
            let meta = self
                .durable
                .manifest()
                .streams
                .get(stream)
                .copied()
                .unwrap_or_default();
            let mut st = self.start_state_pub(&workflow, &node);
            let mut anchored = true;
            let mut offset = match st.get("offset").and_then(Value::as_u64) {
                Some(o) => o,
                None => {
                    // First arm: `from` decides where history begins for THIS
                    // consumer — `new` skips what already happened, `earliest`
                    // replays the retained tail. The anchor must PERSIST even
                    // when nothing fires now: re-deriving "new" on a later
                    // poll would skip every event emitted since this one.
                    anchored = false;
                    let from = spec.get("from").and_then(Value::as_str).unwrap_or("new");
                    if from == "earliest" {
                        meta.first.saturating_sub(1)
                    } else {
                        meta.seq
                    }
                }
            };
            // Retention may have trimmed past a lagging consumer.
            if offset + 1 < meta.first {
                offset = meta.first.saturating_sub(1);
            }
            let mut ids: Vec<Value> = st
                .get("last_ids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // `rate: "<burst>/<per>"` paces CONSUMPTION: matching events stay
            // queued on the stream (durable, in order) and fire as tokens
            // allow — which turns any stream into a worked-off queue. Only an
            // event that WOULD fire spends a token; filtered-out ones pass
            // freely.
            let rate = spec
                .get("rate")
                .and_then(Value::as_str)
                .and_then(|r| crate::supervisor::tree::parse_rate(r).ok());
            let start_offset = offset;
            let mut fired = 0usize;
            while offset < meta.seq && fired < BATCH {
                let next = offset + 1;
                let Some(env) = self
                    .durable
                    .get(Kind::Event, &key(stream, next))
                    .ok()
                    .flatten()
                else {
                    offset = next; // trimmed underneath us — skip forward
                    continue;
                };
                let event = env.state;
                offset = next;
                let subject = event.get("subject").and_then(Value::as_str).unwrap_or("");
                if let Some(pat) = spec.get("subject").and_then(Value::as_str)
                    && !subject_matches(pat, subject)
                {
                    continue;
                }
                // The feedback rule: never fire on events this workflow caused.
                if event.get("source").and_then(Value::as_str) == Some(workflow.as_str()) {
                    continue;
                }
                if let Some(filter) = spec.get("filter").and_then(Value::as_str) {
                    let mut data = crate::engine::template::Data::new();
                    data.insert("event".into(), event.clone());
                    let vars: Vec<(&str, &Value)> =
                        data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    if crate::cel::eval_bool(filter.trim().trim_start_matches("CEL:").trim(), &vars)
                        != Ok(true)
                    {
                        continue;
                    }
                }
                // At-least-once dedup: a replayed append carries the same id.
                let id = event.get("id").cloned().unwrap_or(Value::Null);
                if !id.is_null() && ids.contains(&id) {
                    continue;
                }
                if let Some((burst, secs)) = rate {
                    let key = format!("stream:{workflow}/{node}");
                    let (bucket, _, _) = self.step_rates.entry(key).or_insert_with(|| {
                        (
                            crate::supervisor::tree::TokenBucket::new(burst, burst as f64 / secs),
                            secs,
                            burst,
                        )
                    });
                    if !bucket.try_take() {
                        // Paced out: leave THIS event unconsumed and stop —
                        // the offset write below persists everything before it.
                        offset -= 1;
                        break;
                    }
                }
                self.fire_start(&workflow, &node, &spec, event, "stream");
                if !id.is_null() {
                    ids.push(id);
                    if ids.len() > DEDUP_RING {
                        let drop = ids.len() - DEDUP_RING;
                        ids.drain(..drop);
                    }
                }
                fired += 1;
            }
            if offset != start_offset || !anchored {
                st["offset"] = json!(offset);
                st["last_ids"] = Value::Array(ids);
                self.set_start_state_pub(&workflow, &node, st);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_match_exactly_or_by_prefix_glob() {
        assert!(subject_matches("order.paid", "order.paid"));
        assert!(!subject_matches("order.paid", "order.shipped"));
        assert!(subject_matches("order.*", "order.paid"));
        assert!(subject_matches("order.*", "order.shipped.partial"));
        assert!(!subject_matches("order.*", "invoice.paid"));
        assert!(subject_matches("*", "anything.at.all"));
    }
}
