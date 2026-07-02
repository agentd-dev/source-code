// SPDX-License-Identifier: Apache-2.0
//! The reusable **MCP server** base: transport, framing, connection handling, the
//! lifecycle/version machinery, and the resource-subscription registry — agentd's
//! served self-MCP (and any other embedder's server) builds its domain surface on
//! top by implementing [`Handler`].
//!
//! The split mirrors the client: this module owns the *protocol* (how bytes become
//! requests, how `initialize` / `server/discover` / `ping` are answered across
//! both eras, how a subscriber is pushed a `notifications/resources/updated`),
//! while the embedder owns the *domain* (which tools exist, which resources are
//! readable, who may subscribe to what). One [`Handler`] trait is the seam.
//!
//! Transport is deliberately minimal and dependency-light (RFC 0015 §3.6): a
//! blocking listener, one thread per connection, speaking the same NDJSON JSON-RPC
//! codec ([`crate::rpc::frame`]) as the client. No async, no mio. [`ServeStream`]
//! type-erases unix vs. vsock so the framing, threading, and dispatch are entirely
//! transport-agnostic ("the unix server with the socket type swapped").

use crate::rpc::{Notification, frame};
use crate::wire::method;
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Which transport a connection arrived on, and therefore its trust domain (RFC
/// 0015 §3.3-§3.4). A generic two-domain model the framework only carries and
/// hands to the [`Handler`]; the embedder assigns meaning:
///   * [`Stdio`](PeerOrigin::Stdio) — an in-process / same-trust caller (agentd's
///     own driving harness over the process stdio).
///   * [`Management`](PeerOrigin::Management) — a peer that dialed a listener (unix
///     socket / vsock), i.e. the management trust domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOrigin {
    /// The process's own stdio / an in-process caller (the driving harness).
    Stdio,
    /// A peer on a listener (unix / vsock) — the management trust domain.
    Management,
}

impl PeerOrigin {
    /// Stable lowercase label for logs/metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerOrigin::Stdio => "stdio",
            PeerOrigin::Management => "management",
        }
    }
}

/// The served-MCP transport, type-erased to one concrete enum so the connection
/// registry ([`SharedWriter`], [`Subscriber`]) stays monomorphic across transports
/// while the *same* connection code serves each. Both variants are `Read + Write`
/// with a [`try_clone`](ServeStream::try_clone) (the connection's write half is
/// shared with the threads that push notifications), so the NDJSON framing,
/// threading, and dispatch are entirely transport-agnostic (RFC 0015 §3.2).
pub enum ServeStream {
    /// A unix-domain-socket peer.
    Unix(UnixStream),
    /// An AF_VSOCK peer (host↔guest management transport).
    #[cfg(feature = "vsock")]
    Vsock(vsock::VsockStream),
}

impl ServeStream {
    /// Clone the handle (a second fd onto the same connection) for the shared write
    /// half. Mirrors `UnixStream::try_clone`.
    pub fn try_clone(&self) -> io::Result<ServeStream> {
        match self {
            ServeStream::Unix(s) => s.try_clone().map(ServeStream::Unix),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.try_clone().map(ServeStream::Vsock),
        }
    }

    /// Bound a stalled-but-alive peer so it can't pin the writer Mutex forever.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match self {
            ServeStream::Unix(s) => s.set_write_timeout(dur),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.set_write_timeout(dur),
        }
    }
}

impl Read for ServeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ServeStream::Unix(s) => s.read(buf),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.read(buf),
        }
    }
}

impl Write for ServeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ServeStream::Unix(s) => s.write(buf),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            ServeStream::Unix(s) => s.flush(),
            #[cfg(feature = "vsock")]
            ServeStream::Vsock(s) => s.flush(),
        }
    }
}

/// A connection's shared write half — both replies and pushed notifications go
/// through it, serialized by the Mutex (a reply and a notification can't interleave
/// bytes). The [`ServeStream`] enum keeps this one type across unix + vsock peers.
pub type SharedWriter = Arc<Mutex<ServeStream>>;

/// A peer subscribed to a resource: which connection, and the writer to push a
/// `notifications/resources/updated` to. Opaque — fields are private; construct +
/// mutate a registry through [`register_subscriber`] / [`drop_subscription`] /
/// [`remove_conn_subscriptions`] and fire pushes through the `notify_*` helpers.
pub struct Subscriber {
    conn: u64,
    writer: SharedWriter,
}

/// `uri` → its subscribers. Pushed when a resource changes. `Arc`-shared with the
/// background threads that mutate resource state (a run reaching a terminal status,
/// a reload landing, an event-ring growth).
pub type SubRegistry = Arc<Mutex<HashMap<String, Vec<Subscriber>>>>;

/// Register `conn` (with its `writer`) as a subscriber of `uri`, idempotently — a
/// second subscribe from the same connection is a no-op rather than a duplicate
/// push target. The embedder does its own gating (which URIs are subscribable, who
/// may subscribe) *before* calling this.
pub fn register_subscriber(subs: &SubRegistry, uri: &str, conn: u64, writer: &SharedWriter) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    let list = g.entry(uri.to_string()).or_default();
    if !list.iter().any(|s| s.conn == conn) {
        list.push(Subscriber {
            conn,
            writer: Arc::clone(writer),
        });
    }
}

/// Drop `conn`'s subscription to a single `uri` (the `resources/unsubscribe` path).
/// Prunes the uri entry entirely once its last subscriber leaves.
pub fn drop_subscription(subs: &SubRegistry, uri: &str, conn: u64) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = g.get_mut(uri) {
        list.retain(|s| s.conn != conn);
        if list.is_empty() {
            g.remove(uri);
        }
    }
}

/// Drop every subscription held by a (now-closed) connection — called when a
/// connection's reader loop ends so pushes never target a dead socket.
pub fn remove_conn_subscriptions(subs: &SubRegistry, conn: u64) {
    let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|_uri, list| {
        list.retain(|s| s.conn != conn);
        !list.is_empty()
    });
}

/// Push `notifications/resources/updated{uri}` to every current subscriber of
/// `uri`, **consuming** the subscription list (the resource changes exactly once —
/// e.g. a subagent run reaching its terminal status — so no entry should linger
/// after its one event). Best-effort: a write to a dead peer fails and is cleaned
/// up when that connection's reader loop ends. The lock is released before writing,
/// so a slow/blocked peer can't stall other notifications.
pub fn notify_resource_updated(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let mut g = subs.lock().unwrap_or_else(|e| e.into_inner());
        match g.remove(uri) {
            Some(list) => list.into_iter().map(|s| s.writer).collect(),
            None => return,
        }
    };
    push_updated(&writers, uri);
}

/// Like [`notify_resource_updated`] but **keeps** the subscriber list — for
/// resources that change REPEATEDLY (a run aggregate on each spawn, a warm session
/// on each turn boundary, `config/effective` on each reload, an event ring on each
/// batch). Cloning the writers under the lock (then releasing it before writing)
/// keeps the entry intact for the next emission. Dead peers are pruned when their
/// reader loop ends ([`remove_conn_subscriptions`]).
pub fn notify_resource_updated_keep(subs: &SubRegistry, uri: &str) {
    let writers: Vec<SharedWriter> = {
        let g = subs.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(uri) {
            Some(list) => list.iter().map(|s| Arc::clone(&s.writer)).collect(),
            None => return,
        }
    };
    push_updated(&writers, uri);
}

fn push_updated(writers: &[SharedWriter], uri: &str) {
    let note = Notification::new(method::NOTIFY_RESOURCES_UPDATED, Some(json!({ "uri": uri })));
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, &note);
        }
    }
}

/// Broadcast a payload-free `note` to every DISTINCT writer currently in the
/// registry — for connection-scoped notifications that aren't tied to a single uri
/// (e.g. `notifications/tools/list_changed` after a hot reload changed the tool
/// set). A connection subscribed to several resources is written to once. Dead
/// writers are pruned by their own reader loop.
pub fn broadcast_distinct(subs: &SubRegistry, note: &Notification) {
    let writers: Vec<SharedWriter> = {
        let g = subs.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen: Vec<*const Mutex<ServeStream>> = Vec::new();
        let mut out: Vec<SharedWriter> = Vec::new();
        for list in g.values() {
            for s in list {
                let ptr = Arc::as_ptr(&s.writer);
                if !seen.contains(&ptr) {
                    seen.push(ptr);
                    out.push(Arc::clone(&s.writer));
                }
            }
        }
        out
    };
    for w in writers {
        if let Ok(mut wl) = w.lock() {
            let _ = frame::write_line(&mut *wl, note);
        }
    }
}
