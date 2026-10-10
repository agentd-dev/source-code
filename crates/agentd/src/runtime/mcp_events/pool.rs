// SPDX-License-Identifier: AGPL-3.0-only
//! The events worker pool (RFC 0045 §5.6.1): `events/*` requests run on a few
//! threads of their own, so a slow server never blocks the loop. Each answer
//! goes back to the loop as an event; the loop is the single writer of
//! streams and records.

use super::wire::{self, Descriptor, PollPage, Subscribed};
use crate::mcp::client::McpClient;
use crate::runtime::events::Event;
use serde_json::{Map, Value};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Threads in the pool.
pub const WORKERS: usize = 4;
/// The most `events/list` pages one listing follows.
pub const MAX_LIST_PAGES: usize = 20;
/// The most descriptors one listing keeps.
pub const MAX_DESCRIPTORS: usize = 1000;

/// One request for the pool.
pub enum Job {
    List {
        server: String,
        client: Arc<McpClient>,
        bound: Duration,
    },
    Poll {
        key: String,
        /// Which poll of the binding this is (see `Binding::generation`).
        generation: u64,
        client: Arc<McpClient>,
        params: Map<String, Value>,
        bound: Duration,
    },
    /// A webhook arm or refresh: `events/subscribe`. An arm is a fresh
    /// subscription — `unsubscribe` first (whatever it answers), on the
    /// connection a credential change replaced (`prior`) and on this one —
    /// so the server replays from the cursor (§5.6.3).
    Arm {
        key: String,
        generation: u64,
        server: String,
        client: Arc<McpClient>,
        prior: Option<Arc<McpClient>>,
        /// Fresh: unsubscribe first. The subscription's key either way, for
        /// letting go of what this arm made if its binding is gone by then.
        fresh: bool,
        unsubscribe: Map<String, Value>,
        subscribe: Map<String, Value>,
        bound: Duration,
    },
    /// An eager `events/unsubscribe`: a webhook binding stopped — on the
    /// connection a reload replaced (`prior`), then on `client`.
    Unsubscribe {
        key: String,
        generation: u64,
        server: String,
        client: Option<Arc<McpClient>>,
        prior: Option<Arc<McpClient>>,
        params: Map<String, Value>,
        bound: Duration,
    },
}

/// Why a request got no usable answer.
#[derive(Debug, Clone)]
pub enum Failure {
    /// The server refused it in JSON-RPC: the code decides what follows.
    Rpc(::mcp::rpc::RpcError),
    /// The connection failed, or the answer was not the shape it must be.
    Transport(String),
    /// The listing ran past [`MAX_LIST_PAGES`] or [`MAX_DESCRIPTORS`].
    TooLarge,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Rpc(e) => write!(f, "{} ({})", e.message, e.code),
            Failure::Transport(m) => f.write_str(m),
            Failure::TooLarge => write!(
                f,
                "events/list ran past {MAX_LIST_PAGES} pages or {MAX_DESCRIPTORS} event types"
            ),
        }
    }
}

/// One answer, back to the loop.
pub enum Done {
    List {
        server: String,
        result: Result<Vec<Descriptor>, Failure>,
    },
    Poll {
        key: String,
        generation: u64,
        result: Result<PollPage, Failure>,
    },
    Arm {
        key: String,
        generation: u64,
        /// A fresh subscription, not a refresh.
        fresh: bool,
        result: Result<Subscribed, Failure>,
        /// What made it, to let it go again if its binding is gone.
        server: String,
        client: Arc<McpClient>,
        unsubscribe: Map<String, Value>,
    },
    Unsubscribe {
        key: String,
        generation: u64,
        server: String,
        result: Result<(), Failure>,
    },
}

impl std::fmt::Debug for Done {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Done::List { server, result } => f
                .debug_struct("List")
                .field("server", server)
                .field("ok", &result.is_ok())
                .finish(),
            Done::Poll {
                key,
                generation,
                result,
            } => f
                .debug_struct("Poll")
                .field("key", key)
                .field("generation", generation)
                .field("ok", &result.is_ok())
                .finish(),
            Done::Arm {
                key,
                generation,
                fresh,
                result,
                server,
                ..
            } => f
                .debug_struct("Arm")
                .field("key", key)
                .field("generation", generation)
                .field("fresh", fresh)
                .field("server", server)
                .field("ok", &result.is_ok())
                .finish_non_exhaustive(),
            Done::Unsubscribe {
                key,
                generation,
                server,
                result,
            } => f
                .debug_struct("Unsubscribe")
                .field("key", key)
                .field("generation", generation)
                .field("server", server)
                .field("ok", &result.is_ok())
                .finish(),
        }
    }
}

pub struct Pool {
    jobs: Sender<Job>,
}

impl Pool {
    /// Start the workers; each answer is sent to `events` for the loop.
    pub(crate) fn start(events: Sender<Event>) -> Pool {
        let (jobs, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..WORKERS {
            let rx = Arc::clone(&rx);
            let events = events.clone();
            let _ = std::thread::Builder::new()
                .name(format!("agentd-events-{i}"))
                .spawn(move || work(&rx, &events));
        }
        Pool { jobs }
    }

    pub(crate) fn submit(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

fn work(rx: &Mutex<Receiver<Job>>, events: &Sender<Event>) {
    loop {
        let job = {
            let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
            match rx.recv() {
                Ok(j) => j,
                Err(_) => return,
            }
        };
        let done = match job {
            Job::List {
                server,
                client,
                bound,
            } => Done::List {
                result: list(&client, bound),
                server,
            },
            Job::Poll {
                key,
                generation,
                client,
                params,
                bound,
            } => Done::Poll {
                generation,
                result: call(&client, wire::method::POLL, params, bound).and_then(|v| {
                    serde_json::from_value::<PollPage>(v)
                        .map_err(|e| Failure::Transport(format!("events/poll: {e}")))
                }),
                key,
            },
            Job::Arm {
                key,
                generation,
                server,
                client,
                prior,
                fresh,
                unsubscribe,
                subscribe,
                bound,
            } => Done::Arm {
                result: arm(
                    &client,
                    prior.as_deref(),
                    fresh,
                    &unsubscribe,
                    subscribe,
                    bound,
                ),
                key,
                generation,
                fresh,
                server,
                client,
                unsubscribe,
            },
            Job::Unsubscribe {
                key,
                generation,
                server,
                client,
                prior,
                params,
                bound,
            } => {
                if let Some(prior) = &prior {
                    let _ = call(prior, wire::method::UNSUBSCRIBE, params.clone(), bound);
                }
                Done::Unsubscribe {
                    result: match &client {
                        Some(c) => call(c, wire::method::UNSUBSCRIBE, params, bound).map(|_| ()),
                        None => Ok(()),
                    },
                    key,
                    generation,
                    server,
                }
            }
        };
        if events.send(Event::McpEvents(Box::new(done))).is_err() {
            return;
        }
    }
}

fn call(
    client: &McpClient,
    method: &str,
    params: Map<String, Value>,
    bound: Duration,
) -> Result<Value, Failure> {
    match client.call_raw(method, params, wire::client_extensions(), bound) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Failure::Rpc(e)),
        Err(e) => Err(Failure::Transport(e.to_string())),
    }
}

/// A webhook arm or refresh. A fresh one first lets go of the old
/// subscription — on the replaced connection, whatever it answers, then on
/// this one, which must answer that it did (or that there was none): a
/// subscribe that found the old one still live would be a refresh, whose
/// cursor is a no-op, and nothing abandoned would be replayed.
fn arm(
    client: &McpClient,
    prior: Option<&McpClient>,
    fresh: bool,
    unsubscribe: &Map<String, Value>,
    subscribe: Map<String, Value>,
    bound: Duration,
) -> Result<Subscribed, Failure> {
    if fresh {
        if let Some(prior) = prior {
            let _ = call(prior, wire::method::UNSUBSCRIBE, unsubscribe.clone(), bound);
        }
        match call(
            client,
            wire::method::UNSUBSCRIBE,
            unsubscribe.clone(),
            bound,
        ) {
            Ok(_) => {}
            Err(Failure::Rpc(r)) if r.code == wire::code::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
    }
    let v = call(client, wire::method::SUBSCRIBE, subscribe, bound)?;
    serde_json::from_value::<Subscribed>(v)
        .map_err(|e| Failure::Transport(format!("events/subscribe: {e}")))
}

/// Every page of `events/list`, within the caps.
fn list(client: &McpClient, bound: Duration) -> Result<Vec<Descriptor>, Failure> {
    let mut out: Vec<Descriptor> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let v = call(
            client,
            wire::method::LIST,
            wire::list_params(cursor.as_deref()),
            bound,
        )?;
        let page: wire::ListPage = serde_json::from_value(v)
            .map_err(|e| Failure::Transport(format!("events/list: {e}")))?;
        out.extend(page.events);
        if out.len() > MAX_DESCRIPTORS {
            return Err(Failure::TooLarge);
        }
        match page.next_cursor {
            Some(c) if !c.is_empty() => cursor = Some(c),
            _ => return Ok(out),
        }
    }
    Err(Failure::TooLarge)
}
