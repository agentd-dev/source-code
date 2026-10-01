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

use crate::state::{Kind, kill_point, now_ms};
use crate::store::StoreError;
use serde_json::{Value, json};

/// How many events one consumer advances per reactor pass — bounds the time
/// the single-writer loop spends per tick on one busy stream while still
/// draining a backlog quickly (32/pass ≈ thousands/sec).
const BATCH: usize = 32;
/// Recent event ids each consumer remembers for at-least-once dedup.
const DEDUP_RING: usize = 64;
/// How many occupied event keys one append steps over before it gives up and
/// leaves the rest to the next append. Each is one store write; the bound
/// keeps a long unflushed backlog (or a store that answers "exists" for every
/// key) from holding the single-writer loop.
const HEAD_PROBES: usize = 1024;
/// The line a consumer holding on a store read writes, once per hold — and
/// its mark in `start_held`.
const READ_HELD: &str = "stream.read.failed";

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

/// Why an append was refused.
#[derive(Debug)]
pub(crate) enum AppendError {
    /// The event is larger than `store.max_value_bytes`: no retry of it fits,
    /// so a producer outside this process is told not to send it again.
    TooLarge(String),
    /// Everything else — an undeclared stream, pressure, a store that did not
    /// answer, a head still recovering — may pass on a retry.
    Refused(String),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppendError::TooLarge(m) | AppendError::Refused(m) => f.write_str(m),
        }
    }
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
        self.append(stream, subject, correlation, data, id, source)
            .map_err(|e| e.to_string())
    }

    fn append(
        &mut self,
        stream: &str,
        subject: &str,
        correlation: Option<&str>,
        data: Value,
        id: &str,
        source: &str,
    ) -> Result<u64, AppendError> {
        let Some(cfg) = self.settings.streams.get(stream).cloned() else {
            return Err(AppendError::Refused(format!(
                "stream {stream:?} is not declared (add it under `streams:`)"
            )));
        };
        if let Some(cause) = self.pressure.refusal(false) {
            return Err(AppendError::Refused(format!("emit refused: {cause}")));
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
        // The head in the manifest can be behind the store: events are written
        // at once, the manifest is flushed debounced, and a crash between the
        // two leaves events past the head that the next life does not know
        // about. Each one may have been acknowledged. So the event key is
        // create-only, and a key that is already taken moves the head past
        // it: the event that was there is kept, and becomes visible because
        // the head now covers it.
        let head = meta.seq;
        let mut seq = head + 1;
        let mut probes = 0usize;
        loop {
            let event = json!({
                "id": id, "stream": stream, "subject": subject, "seq": seq,
                "ts": now_ms(), "source": source,
                "correlation": correlation, "data": &data,
            });
            let created = self
                .durable
                .create(Kind::Event, &key(stream, seq), event)
                .map_err(|e| {
                    let msg = format!("stream {stream:?} append: {e}");
                    match e {
                        StoreError::TooLarge { .. } => AppendError::TooLarge(msg),
                        _ => AppendError::Refused(msg),
                    }
                })?;
            if created.is_some() {
                break;
            }
            probes += 1;
            if probes == HEAD_PROBES {
                // A gap this long is a backlog the next append finishes, not
                // a reason to block the loop: keep what was found (those
                // events exist) and refuse this one, so its sender retries.
                meta.seq = seq;
                self.durable.manifest_update(|m| {
                    m.streams.insert(stream.to_string(), meta);
                });
                self.log.warn(
                    "stream.head.recovered",
                    json!({"stream": stream, "from": head + 1, "to": seq, "complete": false}),
                );
                // The head now covers events no consumer has seen: wake them
                // in this iteration, as a successful append does.
                self.stream_dirty = true;
                return Err(AppendError::Refused(format!(
                    "stream {stream:?} append: the stream head is still recovering \
                     (seq {} to {seq} were already written); retry",
                    head + 1
                )));
            }
            seq += 1;
        }
        kill_point("stream.after_append");
        if seq > head + 1 {
            self.log.warn(
                "stream.head.recovered",
                json!({"stream": stream, "from": head + 1, "to": seq - 1, "complete": true}),
            );
        }
        meta.seq = seq;
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

    /// Append an event whose producer is answered OUTSIDE this process — the
    /// `2xx` of a webhook `into:`.
    ///
    /// The event record is durable when [`append_event`](Self::append_event)
    /// returns, but the stream head that makes consumers see it is in the
    /// debounced manifest. A crash before that flush would leave an event the
    /// sender was told was kept and that no consumer reaches until some later
    /// append steps over it. So the head is flushed first, and a head that
    /// cannot be saved is a refused append: the sender retries, and the copy
    /// it appends carries the same id, which consumers deduplicate.
    #[cfg(feature = "a2a")]
    pub(crate) fn append_event_acked(
        &mut self,
        stream: &str,
        subject: &str,
        correlation: Option<&str>,
        data: Value,
        id: &str,
        source: &str,
    ) -> Result<u64, AppendError> {
        let seq = self.append(stream, subject, correlation, data, id, source)?;
        self.durable.flush(true).map_err(|e| {
            AppendError::Refused(format!(
                "stream {stream:?} append: the stream head was not saved: {e}"
            ))
        })?;
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
    /// Every armed start node of `kind` — the shared shape `stream` and
    /// `correlate` both consume by.
    fn stream_consumers(
        &self,
        kind: &str,
    ) -> Vec<(String, String, serde_json::Map<String, Value>)> {
        self.workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| s.kind == kind)
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub(crate) fn poll_stream_starts(&mut self) {
        // A hold belongs to an armed consumer. One that a reload removed or
        // disarmed would otherwise keep its entry, and the first refusal of a
        // consumer re-armed under the same name would go unsaid.
        let armed: std::collections::HashSet<String> = ["stream", "correlate"]
            .into_iter()
            .flat_map(|k| self.stream_consumers(k))
            .map(|(w, n, _)| format!("{w}.{n}"))
            .collect();
        self.start_held.retain(|k, _| armed.contains(k));
        let consumers = self.stream_consumers("stream");
        let mut lags = Vec::with_capacity(consumers.len());
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
            // Taken before the trim check below, so that skipping forward is
            // itself a change that persists: otherwise every pass would find
            // the same gap and report it again.
            let start_offset = offset;
            let mut skipped = Skipped::default();
            // Retention may have trimmed past a lagging consumer.
            if offset + 1 < meta.first {
                skipped.note(offset + 1, meta.first - 1);
                offset = meta.first - 1;
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
            // `batch: {size, window}` groups matching events into ONE run.
            // The partial batch lives in the durable start-state alongside the
            // offset, so a restart mid-batch resumes it rather than re-reading
            // events the offset has already passed.
            let batch_size = spec
                .get("batch")
                .and_then(|b| b.get("size"))
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            let batch_window_ms = spec
                .get("batch")
                .and_then(|b| b.get("window"))
                .and_then(Value::as_str)
                .and_then(|d| crate::config::parse_duration(d).ok())
                .map(|d| d.as_millis() as u64);
            let mut batch: Vec<Value> = st
                .get("batch")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut batch_since = st.get("batch_since").and_then(Value::as_u64);
            let mut fired = 0usize;
            let mut gone = 0usize;
            while offset < meta.seq && fired + gone < BATCH {
                let next = offset + 1;
                let event = match self.consumer_read(stream, &workflow, &node, next) {
                    Read::Event(event) => event,
                    // Gone underneath us: as lost as a trimmed event, and
                    // reported with them. Counted against the pass, so a long
                    // run of missing records is still worked off in bounded
                    // steps.
                    Read::Gone => {
                        skipped.note(next, next);
                        offset = next;
                        gone += 1;
                        continue;
                    }
                    // The store did not answer: the event may well be there,
                    // so the consumer holds here, as it does for a refusal.
                    Read::Unavailable => break,
                };
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
                // A refused firing is handled exactly like being paced out:
                // the event stays on the stream and is offered again on a
                // later pass. Shed, frozen and a failed inbox write are all
                // temporary, and moving the offset past them would lose the
                // event for good.
                let admitted = match batch_size {
                    Some(n) => {
                        batch_since.get_or_insert_with(now_ms);
                        batch.push(event);
                        if batch.len() >= n {
                            let outcome =
                                self.fire_batch(&workflow, &node, &spec, stream, &batch, true);
                            if outcome.consumed() {
                                batch.clear();
                                batch_since = None;
                            } else {
                                // The rest of the batch is durable start-state
                                // already; only the event that filled it goes
                                // back to the stream.
                                batch.pop();
                                if batch.is_empty() {
                                    batch_since = None;
                                }
                            }
                            outcome.consumed()
                        } else {
                            true
                        }
                    }
                    None => {
                        let origin = json!({"stream": stream, "seq": next, "event_id": id});
                        self.fire_stream_start(&workflow, &node, &spec, event, "stream", origin)
                            .consumed()
                    }
                };
                if !admitted {
                    offset -= 1;
                    break;
                }
                if !id.is_null() {
                    ids.push(id);
                    if ids.len() > DEDUP_RING {
                        let drop = ids.len() - DEDUP_RING;
                        ids.drain(..drop);
                    }
                }
                fired += 1;
            }
            // A batch that never fills would otherwise hold events for ever on
            // a quiet stream: `window` is what bounds that latency, and the
            // sweep runs every tick rather than only when an event arrives —
            // the last event of a burst must not wait for the next burst. A
            // refused partial batch stays as it is and is offered again by the
            // next sweep.
            if let (Some(_), Some(win), Some(since)) = (batch_size, batch_window_ms, batch_since)
                && !batch.is_empty()
                && now_ms().saturating_sub(since) >= win
                && self
                    .fire_batch(&workflow, &node, &spec, stream, &batch, false)
                    .consumed()
            {
                batch.clear();
                batch_since = None;
            }
            self.log_skipped(stream, &workflow, &node, &skipped);
            lags.push((
                stream.to_string(),
                format!("{workflow}/{node}"),
                meta.seq.saturating_sub(offset),
            ));
            let batch_changed = batch_size.is_some()
                && (st.get("batch") != Some(&Value::Array(batch.clone()))
                    || st.get("batch_since").and_then(Value::as_u64) != batch_since);
            if offset != start_offset || !anchored || batch_changed {
                st["offset"] = json!(offset);
                st["last_ids"] = Value::Array(ids);
                if batch_size.is_some() {
                    st["batch"] = Value::Array(batch);
                    st["batch_since"] = match batch_since {
                        Some(t) => json!(t),
                        None => Value::Null,
                    };
                }
                self.set_start_state_pub(&workflow, &node, st);
            }
        }
        crate::obs::metrics::set_stream_lag("stream", lags);
    }

    /// Offer a batch as ONE run. `full` says whether `size` filled it or its
    /// `window` ran out, so a run can tell the two apart.
    fn fire_batch(
        &mut self,
        workflow: &str,
        node: &str,
        spec: &serde_json::Map<String, Value>,
        stream: &str,
        batch: &[Value],
        full: bool,
    ) -> super::starts::Admission {
        let seq = |e: Option<&Value>| e.and_then(|e| e.get("seq")).cloned();
        // Every event by name: a batch that is discarded (`inputs_invalid`,
        // `too_large`) has this line as its only record, and the range alone
        // cannot say which events a filtered stream put in it.
        let field = |f: &str| -> Vec<Value> {
            batch
                .iter()
                .map(|e| e.get(f).cloned().unwrap_or(Value::Null))
                .collect()
        };
        let origin = json!({
            "stream": stream, "from": seq(batch.first()), "to": seq(batch.last()),
            "events": batch.len(), "seqs": field("seq"), "event_ids": field("id"),
        });
        let payload = json!({"events": batch, "count": batch.len(), "full": full});
        self.fire_stream_start(workflow, node, spec, payload, "stream", origin)
    }

    /// Read the event at `seq` for the consumer `workflow`/`node`.
    ///
    /// A store that did not answer is not a missing event: moving past it
    /// would consume, unfired, an event that is still there — during an
    /// outage, a consumer's whole backlog in one pass. So it holds the
    /// consumer, said once per hold like a refused firing; the next read that
    /// answers lifts it. A record that is there but cannot be parsed is as
    /// lost as a trimmed one, and is named before it is skipped.
    fn consumer_read(&mut self, stream: &str, workflow: &str, node: &str, seq: u64) -> Read {
        let held = format!("{workflow}.{node}");
        let read = match self.durable.get(Kind::Event, &key(stream, seq)) {
            Ok(Some(env)) => Read::Event(env.state),
            Ok(None) => Read::Gone,
            Err(e) => return self.consumer_read_failed(stream, workflow, node, seq, e),
        };
        if self.start_held.get(&held) == Some(&READ_HELD) {
            self.start_held.remove(&held);
        }
        read
    }

    fn consumer_read_failed(
        &mut self,
        stream: &str,
        workflow: &str,
        node: &str,
        seq: u64,
        err: StoreError,
    ) -> Read {
        match err {
            StoreError::Corrupt(e) => {
                self.log.warn(
                    "stream.event.corrupt",
                    json!({"stream": stream, "seq": seq, "workflow": workflow,
                           "node": node, "err": e}),
                );
                Read::Gone
            }
            e => {
                if self
                    .start_held
                    .insert(format!("{workflow}.{node}"), READ_HELD)
                    != Some(READ_HELD)
                {
                    self.log.warn(
                        READ_HELD,
                        json!({"stream": stream, "seq": seq, "workflow": workflow,
                               "node": node, "err": e.to_string()}),
                    );
                }
                Read::Unavailable
            }
        }
    }

    /// Say that retention (or a missing record) took events this consumer had
    /// not read. The consumer cannot get them back, but the operator can size
    /// `retention` or `rate` for next time — and only if the loss is visible.
    fn log_skipped(&self, stream: &str, workflow: &str, node: &str, skipped: &Skipped) {
        if let Some(from) = skipped.from {
            self.log.warn(
                "stream.consumer.skipped",
                json!({"stream": stream, "workflow": workflow, "node": node,
                       "from": from, "to": skipped.to, "events": skipped.events}),
            );
        }
    }
}

/// What a consumer found at one seq.
enum Read {
    Event(Value),
    /// Nothing is there, or nothing readable: no retry brings it back.
    Gone,
    /// The store did not answer; the event may be there.
    Unavailable,
}

/// The events one consumer pass moved past without reading: the range they
/// span and how many there were.
#[derive(Default)]
struct Skipped {
    from: Option<u64>,
    to: u64,
    events: u64,
}

impl Skipped {
    fn note(&mut self, from: u64, to: u64) {
        self.from.get_or_insert(from);
        self.to = to;
        self.events += to - from + 1;
    }
}

/// The just-appended event a `forward:` describes — the identity that travels
/// together through both forward paths.
pub(crate) struct Forwarded<'a> {
    pub stream: &'a str,
    pub subject: &'a str,
    pub id: &'a str,
    pub seq: u64,
    pub run_id: &'a str,
    pub step_id: &'a str,
}

impl crate::runtime::reactor::Runtime {
    /// Push a just-appended event to a fleet PEER — `emit`'s
    /// `forward: {peer: name}` (RFC 0035 §5).
    ///
    /// The peer receives it as an ordinary A2A message carrying a
    /// `stream.forwarded` command, which its own `a2a` start can bind straight
    /// back onto a stream with `into:`. That is the whole cross-instance story:
    /// one instance's `emit` becomes another's stream event, over mTLS or the
    /// co-located unix-socket lane, with each side's durable copy independent.
    ///
    /// Same posture as the webhook forward — off the reactor, best effort, the
    /// append already stands.
    #[cfg(feature = "a2a")]
    pub(crate) fn forward_event_peer(&mut self, peer: &str, ev: &Forwarded<'_>, data: &Value) {
        let Forwarded {
            stream,
            subject,
            id,
            seq,
            run_id,
            step_id,
        } = *ev;
        let timeout = std::time::Duration::from_secs(30);
        let (endpoint, auth) = match self.a2a_peer_conn_pub(peer, timeout, "stream forward") {
            Ok(v) => v,
            Err(e) => {
                self.log.warn(
                    "stream.forward.refused",
                    json!({"run": run_id, "step": step_id, "stream": stream, "err": e}),
                );
                return;
            }
        };
        // The fields sit directly on the `agentd` object, not nested under a
        // second `args`: the receiving `a2a` start defines `args` AS that
        // object with `op` removed, so a nested key would land the payload at
        // `output.args.args.*`.
        let parts = json!([{"data": {"agentd": {
            "op": "stream.forwarded",
            "stream": stream, "subject": subject, "id": id, "seq": seq, "data": data,
        }}}]);
        let log = self.log.clone();
        let (st, sub, peer_name) = (stream.to_string(), subject.to_string(), peer.to_string());
        let msg_id = id.to_string();
        std::thread::Builder::new()
            .name("stream:forward:peer".into())
            .spawn(move || {
                let deadline = std::time::Instant::now() + timeout;
                if let Err(e) = crate::mcp::a2a_client::send(
                    &endpoint,
                    auth,
                    &parts,
                    None,
                    Some(&msg_id),
                    deadline,
                ) {
                    log.warn(
                        "stream.forward.failed",
                        json!({"stream": st, "subject": sub, "seq": seq, "peer": peer_name,
                               "err": e, "durable": true}),
                    );
                }
            })
            .ok();
    }

    /// Push a just-appended event to an outbound webhook — `emit`'s
    /// `forward: {webhook: URL}` (RFC 0035 §5).
    ///
    /// Fire-and-forget on its own thread, like every other outbound dial in the
    /// daemon: the durable append already happened and IS the source of truth,
    /// so a slow or dead receiver must not hold the single-writer loop or fail
    /// the step. A consumer that misses the push still reads the event from its
    /// offset — the push is a latency optimisation, not the delivery.
    ///
    /// It is a covered egress surface, judged by the same `egress_allows` rule
    /// as the `http` node. Forwarding would otherwise be a way to reach a host
    /// the egress policy refuses, simply by appending an event on the way.
    pub(crate) fn forward_event(&mut self, url: &str, ev: &Forwarded<'_>, allow_private: bool) {
        let Forwarded {
            stream,
            subject,
            id,
            seq,
            run_id,
            step_id,
        } = *ev;
        use crate::config::settings;
        if let Err(e) = settings::egress_allows(
            &self.settings.services,
            self.settings.security.egress,
            settings::ServiceKind::Http,
            url,
        ) {
            self.log.warn(
                "stream.forward.refused",
                json!({"run": run_id, "step": step_id, "stream": stream, "err": e}),
            );
            return;
        }
        let event = json!({
            "id": id, "stream": stream, "subject": subject, "seq": seq,
        });
        let body = serde_json::to_vec(&event).unwrap_or_default();
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "user-agent".to_string(),
                format!("agentd/{}", crate::VERSION),
            ),
        ];
        let log = self.log.clone();
        let (url, st, sub) = (url.to_string(), stream.to_string(), subject.to_string());
        std::thread::Builder::new()
            .name("stream:forward".into())
            .spawn(move || {
                let outcome = crate::runtime::http_node::do_http(
                    &url,
                    "POST",
                    "",
                    &headers,
                    &body,
                    std::time::Duration::from_secs(30),
                    allow_private,
                );
                // Logged either way at the level the outcome deserves: a
                // forward that failed is not a lost event (the append stands),
                // but it IS something an operator watching a downstream
                // integration needs to see.
                match outcome {
                    Ok(v) if (200..400).contains(&v["status"].as_u64().unwrap_or(0)) => {}
                    Ok(v) => log.warn(
                        "stream.forward.failed",
                        json!({"stream": st, "subject": sub, "seq": seq,
                               "status": v["status"], "durable": true}),
                    ),
                    Err(e) => log.warn(
                        "stream.forward.failed",
                        json!({"stream": st, "subject": sub, "seq": seq, "err": e,
                               "durable": true}),
                    ),
                }
            })
            .ok();
    }
}

/// Resolve `by` against an event: a dot path, defaulting to the envelope's own
/// `correlation` field (which `emit` already carries, so the common case needs
/// no path at all).
fn correlation_of(event: &Value, by: &str) -> Option<String> {
    let mut cur = event;
    for seg in by.split('.') {
        cur = cur.get(seg)?;
    }
    match cur {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        // A number or bool is a legitimate join key; anything structured is not.
        other if !other.is_object() && !other.is_array() => Some(other.to_string()),
        _ => None,
    }
}

impl crate::runtime::reactor::Runtime {
    /// Advance every `correlate` start: a multi-event join over one stream.
    ///
    /// `depends_on` joins steps; this joins *events*. It consumes the stream
    /// exactly as a `stream` start does — same offset, same dedup, same
    /// feedback rule — but instead of firing per event it accumulates each one
    /// under its correlation value until every subject in `on` has arrived,
    /// then fires once with the whole set.
    ///
    /// The half-collected sets live in the durable start-state, so a restart
    /// resumes a join rather than losing it. That is also the hazard: a
    /// correlation value whose partner never arrives would be kept for ever,
    /// which is why `window` is mandatory and `max_pending` is enforced.
    pub(crate) fn poll_correlate_starts(&mut self) {
        let consumers = self.stream_consumers("correlate");
        let mut lags = Vec::with_capacity(consumers.len());
        for (workflow, node, spec) in consumers {
            let Some(stream) = spec.get("stream").and_then(Value::as_str) else {
                continue;
            };
            let subjects: Vec<String> = spec
                .get("on")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if subjects.is_empty() {
                continue;
            }
            let by = spec
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("correlation");
            let window_ms = spec
                .get("window")
                .and_then(Value::as_str)
                .and_then(|d| crate::config::parse_duration(d).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(u64::MAX);
            let fire_partial =
                spec.get("on_incomplete").and_then(Value::as_str) == Some("fire_partial");
            let max_pending = spec
                .get("max_pending")
                .and_then(Value::as_u64)
                .unwrap_or(1000) as usize;

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
                    anchored = false;
                    // A join replays by default: `earliest` is the useful
                    // reading, because half of a pair may already be on the
                    // stream when this node arms.
                    let from = spec
                        .get("from")
                        .and_then(Value::as_str)
                        .unwrap_or("earliest");
                    if from == "earliest" {
                        meta.first.saturating_sub(1)
                    } else {
                        meta.seq
                    }
                }
            };
            let start_offset = offset;
            let mut skipped = Skipped::default();
            if offset + 1 < meta.first {
                skipped.note(offset + 1, meta.first - 1);
                offset = meta.first - 1;
            }
            let mut pending: serde_json::Map<String, Value> = st
                .get("pending")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut ids: Vec<Value> = st
                .get("last_ids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // What is due goes first: a set that completed on an earlier pass
            // but was not admitted then (shed, frozen, a failed inbox write),
            // and a set whose window has run out. A consumer holding a refused
            // set does not walk the stream at all, exactly as a `stream`
            // consumer holds its offset: the stream stays the queue, so a hold
            // neither grows `pending` past `max_pending` nor lets a new event
            // for a held correlation land in a slot the refused set then
            // overwrites.
            let due = self.due_sets(
                &workflow,
                &node,
                stream,
                &mut pending,
                &subjects,
                window_ms,
                fire_partial,
            );
            let mut held = !self.offer_sets(
                &workflow,
                &node,
                &spec,
                stream,
                &subjects,
                due,
                &mut pending,
            );
            let mut consumed = 0usize;
            while !held && offset < meta.seq && consumed < BATCH {
                let next = offset + 1;
                consumed += 1;
                let event = match self.consumer_read(stream, &workflow, &node, next) {
                    Read::Event(event) => event,
                    Read::Gone => {
                        skipped.note(next, next);
                        offset = next;
                        continue;
                    }
                    Read::Unavailable => break,
                };
                offset = next;
                // The same admission rules a `stream` start applies, in the
                // same order — a join must not see events a consumer would not.
                if event.get("source").and_then(Value::as_str) == Some(workflow.as_str()) {
                    continue;
                }
                let subject = event.get("subject").and_then(Value::as_str).unwrap_or("");
                let Some(matched) = subjects
                    .iter()
                    .find(|pat| subject_matches(pat, subject))
                    .cloned()
                else {
                    continue;
                };
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
                let id = event.get("id").cloned().unwrap_or(Value::Null);
                if !id.is_null() && ids.contains(&id) {
                    continue;
                }
                let Some(corr) = correlation_of(&event, by) else {
                    // An event that cannot be keyed cannot be joined. Say so
                    // once per event rather than dropping it in silence: a
                    // mistyped `by` otherwise looks exactly like a stream that
                    // never delivers.
                    self.log.warn(
                        "correlate.unkeyed",
                        json!({"workflow": workflow, "node": node, "stream": stream,
                               "subject": subject, "by": by}),
                    );
                    continue;
                };
                if !id.is_null() {
                    ids.push(id);
                    if ids.len() > DEDUP_RING {
                        let drop = ids.len() - DEDUP_RING;
                        ids.drain(..drop);
                    }
                }
                let slot = pending
                    .entry(corr.clone())
                    .or_insert_with(|| json!({"first_ms": now_ms(), "events": {}}));
                slot["events"][&matched] = event;
                let complete = slot["events"]
                    .as_object()
                    .is_some_and(|got| subjects.iter().all(|s| got.contains_key(s)));
                if complete {
                    // Offered the moment it completes. A refused set stays in
                    // `pending` — the event that completed it is consumed into
                    // it — and the walk stops here, so nothing after it can
                    // reach the same correlation before the set is admitted.
                    if let Some(done) = pending.remove(&corr) {
                        held = !self.offer_sets(
                            &workflow,
                            &node,
                            &spec,
                            stream,
                            &subjects,
                            vec![(corr, done)],
                            &mut pending,
                        );
                    }
                    continue;
                }
                // Bound the durable state. Refusing the NEWEST key rather than
                // evicting an old one keeps a half-collected join that may
                // still complete, and makes the refusal visible instead of
                // silently losing a join that was nearly done.
                if pending.len() > max_pending {
                    pending.remove(&corr);
                    self.log.warn(
                        "correlate.overflow",
                        json!({"workflow": workflow, "node": node, "stream": stream,
                               "max_pending": max_pending, "dropped": corr}),
                    );
                }
            }

            // A set whose window ran out while this pass walked. Its fire
            // waits for nothing: the window sweep runs every tick, which is
            // what makes `on_incomplete: fire_partial` escalate on time.
            if !held {
                let due = self.due_sets(
                    &workflow,
                    &node,
                    stream,
                    &mut pending,
                    &subjects,
                    window_ms,
                    fire_partial,
                );
                self.offer_sets(
                    &workflow,
                    &node,
                    &spec,
                    stream,
                    &subjects,
                    due,
                    &mut pending,
                );
            }
            self.log_skipped(stream, &workflow, &node, &skipped);
            lags.push((
                stream.to_string(),
                format!("{workflow}/{node}"),
                meta.seq.saturating_sub(offset),
            ));

            if offset != start_offset
                || !anchored
                || st.get("pending") != Some(&Value::Object(pending.clone()))
            {
                st["offset"] = json!(offset);
                st["last_ids"] = Value::Array(ids);
                st["pending"] = Value::Object(pending);
                self.set_start_state_pub(&workflow, &node, st);
            }
        }
        crate::obs::metrics::set_stream_lag("correlate", lags);
    }

    /// Take out of `pending` every set that is due: complete ones (a set is
    /// only left complete there when its firing was refused), and ones whose
    /// window has run out — fired partial (the escalation shape: "paid but
    /// never shipped" IS the event) or discarded with a line. A complete set
    /// is never treated as an expired one, whatever its age.
    #[allow(clippy::too_many_arguments)]
    fn due_sets(
        &self,
        workflow: &str,
        node: &str,
        stream: &str,
        pending: &mut serde_json::Map<String, Value>,
        subjects: &[String],
        window_ms: u64,
        fire_partial: bool,
    ) -> Vec<(String, Value)> {
        let now = now_ms();
        let due: Vec<(String, bool)> = pending
            .iter()
            .filter_map(|(k, v)| {
                let complete = v["events"]
                    .as_object()
                    .is_some_and(|got| subjects.iter().all(|s| got.contains_key(s)));
                let expired = v
                    .get("first_ms")
                    .and_then(Value::as_u64)
                    .is_some_and(|t| now.saturating_sub(t) >= window_ms);
                (complete || expired).then(|| (k.clone(), complete))
            })
            .collect();
        let mut ready = Vec::new();
        for (corr, complete) in due {
            let Some(set) = pending.remove(&corr) else {
                continue;
            };
            if complete || fire_partial {
                ready.push((corr, set));
            } else {
                self.log.info(
                    "correlate.expired",
                    json!({"workflow": workflow, "node": node, "stream": stream,
                           "correlation": corr, "window_ms": window_ms}),
                );
            }
        }
        ready
    }

    /// Offer each set as one run. The events of a set are consumed from the
    /// stream already — they live in the durable `pending` — so one that is
    /// not admitted goes back there, intact, and is offered again on a later
    /// pass. Once one is refused the rest would be too: they go back with it
    /// rather than each writing the same refusal. `false` when anything went
    /// back: the consumer is held.
    #[allow(clippy::too_many_arguments)]
    fn offer_sets(
        &mut self,
        workflow: &str,
        node: &str,
        spec: &serde_json::Map<String, Value>,
        stream: &str,
        subjects: &[String],
        sets: Vec<(String, Value)>,
        pending: &mut serde_json::Map<String, Value>,
    ) -> bool {
        let mut refused = false;
        for (corr, set) in sets {
            if refused {
                pending.insert(corr, set);
                continue;
            }
            let events: Vec<Value> = subjects
                .iter()
                .filter_map(|s| set["events"].get(s).cloned())
                .collect();
            let missing: Vec<&String> = subjects
                .iter()
                .filter(|s| set["events"].get(s.as_str()).is_none())
                .collect();
            // Every event by name: a discarded set has this line as its only
            // record.
            let field = |f: &str| -> Vec<Value> {
                events
                    .iter()
                    .map(|e| e.get(f).cloned().unwrap_or(Value::Null))
                    .collect()
            };
            let origin = json!({
                "stream": stream, "correlation": corr, "events": events.len(),
                "seqs": field("seq"), "event_ids": field("id"),
            });
            let payload = json!({
                "correlation": corr,
                "events": events,
                "complete": missing.is_empty(),
                "missing": missing,
            });
            let outcome =
                self.fire_stream_start(workflow, node, spec, payload, "correlate", origin);
            if !outcome.consumed() {
                refused = true;
                pending.insert(corr, set);
            }
        }
        !refused
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
