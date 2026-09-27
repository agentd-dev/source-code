// SPDX-License-Identifier: AGPL-3.0-only
//! `SubscribeToEvents`: the observation feed as an SSE stream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::a2a::Principal;
use crate::runtime::a2a_server::SharedFeed;

/// `SubscribeToEvents`: agentd's own stream, not the spec's.
///
/// A `hello` frame states the cursor the client resumed from and whether that
/// cursor still exists — a cursor evicted from the replay window comes back
/// `resync`, meaning re-bootstrap. Then the events the principal may see, and
/// finally a `goodbye` carrying the cursor to resume from, so a reconnect is a
/// continuation rather than a restart.
pub(super) fn feed_stream(
    feed: Arc<SharedFeed>,
    id: Value,
    params: Value,
    principal: Principal,
    deadline: Duration,
) -> Response {
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
            let (events, next) = feed.since(cursor, &who, is_op, 256);
            cursor = next;
            for ev in events {
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
