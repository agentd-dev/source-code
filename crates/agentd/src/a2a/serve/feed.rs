// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd.events/SubscribeToEvents`: the observation feed as an SSE stream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::LivenessCheck;
use crate::a2a::Principal;
use crate::runtime::a2a_server::SharedFeed;
use crate::runtime::surface::{Active, Ext, TASK_ANNOTATIONS_EXTENSION};

/// `agentd.events/SubscribeToEvents`: agentd's own stream, not the spec's.
///
/// A `hello` frame states the cursor the client resumed from and whether that
/// cursor still exists — a cursor evicted from the replay window comes back
/// `resync`, meaning re-bootstrap. Then the events the principal may see, and
/// finally a `goodbye` carrying the cursor to resume from, so a reconnect is a
/// continuation rather than a restart.
///
/// `alive` is the caller's session check, when it signed in with one: once it
/// answers `false` the stream sends no further event and ends with
/// `goodbye{reason: "revoked"}` within one tick — a revoked session keeps
/// nothing it already opened, and is told why rather than left to reconnect
/// with a token that no longer works.
///
/// `active` is what the subscriber activated. The ring holds one copy of each
/// event for every subscriber, so a `task` event is stored annotated and the
/// annotations are taken off here, per subscriber, unless this one activated
/// task-annotations/v1.
pub(super) fn feed_stream(
    feed: Arc<SharedFeed>,
    id: Value,
    params: Value,
    principal: Principal,
    deadline: Duration,
    alive: Option<LivenessCheck>,
    active: Active,
) -> Response {
    let annotated = active.contains(Ext::TaskAnnotations);
    let after = params
        .get("fromSeq")
        .or_else(|| params.get("after"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let (newest, oldest, dropped) = feed.bounds();
    // The cursor predates the replay window: events were evicted past it, so
    // replay from the window start and tell the client to re-bootstrap.
    let evicted = after > 0 && dropped > 0 && after < oldest.saturating_sub(1);
    // The cursor is *ahead* of the feed, which is what every attached client
    // holds across a daemon restart: the feed is in-memory and its seq begins
    // again at 0. Honouring such a cursor silently kills the subscription —
    // `since` only ever yields `seq > cursor`, so the client would sit through a
    // whole restart's worth of events seeing nothing and never learn why.
    let ahead = after > newest;
    let resync = evicted || ahead;
    let start = if resync { 0 } else { after };

    let (tx, rx) = tokio::sync::mpsc::channel::<axum::response::sse::Event>(64);
    let is_op = principal.is_operator();
    let who = principal.id.clone();
    tokio::spawn(async move {
        let hello = json!({"hello": {
            "seq": newest,
            "resume": after,
            "resync": resync,
            "debug": feed.debug(),
            "version": crate::VERSION,
        }});
        if tx.send(frame(&id, hello)).await.is_err() {
            return;
        }
        let mut cursor = start;
        let end = Instant::now() + deadline;
        loop {
            // Asked before anything is read, so a revoked caller is sent
            // nothing past the revocation.
            if alive.as_ref().is_some_and(|check| !check()) {
                let bye = json!({"goodbye": {"seq": cursor, "reason": "revoked"}});
                let _ = tx.send(frame(&id, bye)).await;
                return;
            }
            let (events, next) = feed.since(cursor, &who, is_op, 256);
            cursor = next;
            for mut ev in events {
                if !annotated {
                    strip_annotations(&mut ev);
                }
                if tx.send(frame(&id, json!({"event": ev}))).await.is_err() {
                    return; // the client went away
                }
            }
            if Instant::now() >= end {
                let bye = json!({"goodbye": {"seq": cursor, "reason": "deadline"}});
                let _ = tx.send(frame(&id, bye)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let stream =
        tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

fn frame(id: &Value, payload: Value) -> axum::response::sse::Event {
    axum::response::sse::Event::default().data(
        serde_json::to_string(&json!({"jsonrpc": "2.0", "id": id, "result": payload}))
            .unwrap_or_default(),
    )
}

/// `ev` without the task annotations its task carries, if it carries a task:
/// the metadata key goes, and the metadata with it when nothing else is in it,
/// as a task projected without the extension has none.
fn strip_annotations(ev: &mut Value) {
    let Some(task) = ev.pointer_mut("/data/task").and_then(Value::as_object_mut) else {
        return;
    };
    let emptied = match task.get_mut("metadata").and_then(Value::as_object_mut) {
        Some(meta) => {
            meta.remove(TASK_ANNOTATIONS_EXTENSION);
            meta.is_empty()
        }
        None => false,
    };
    if emptied {
        task.remove("metadata");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task event loses exactly the annotations: the rest of its task, any
    /// other metadata, and every other event are untouched.
    #[test]
    fn a_task_event_is_stripped_of_its_annotations_and_nothing_else() {
        let mut ev = json!({"seq": 3, "kind": "task", "data": {"task": {
            "id": "t", "history": [{"messageId": "m"}],
            "metadata": {(TASK_ANNOTATIONS_EXTENSION): {"principal": "user:a"}},
        }}});
        strip_annotations(&mut ev);
        assert_eq!(
            ev,
            json!({"seq": 3, "kind": "task", "data": {"task": {
                "id": "t", "history": [{"messageId": "m"}],
            }}})
        );
        let mut other = json!({"data": {"task": {"id": "t", "metadata": {
            (TASK_ANNOTATIONS_EXTENSION): {}, "urn:x": 1}}}});
        strip_annotations(&mut other);
        assert_eq!(other["data"]["task"]["metadata"], json!({"urn:x": 1}));
        let mut run = json!({"kind": "run", "data": {"id": "r", "metadata": {"k": 1}}});
        let before = run.clone();
        strip_annotations(&mut run);
        assert_eq!(run, before);
    }
}
