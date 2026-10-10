// SPDX-License-Identifier: AGPL-3.0-only
//! The events worker pool (RFC 0045 §5.6.1): `events/*` requests run on a few
//! threads of their own, so a slow server never blocks the loop. Each answer
//! goes back to the loop as an event; the loop is the single writer of
//! streams and records.

use super::wire::{self, Descriptor, PollPage};
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
        client: Arc<McpClient>,
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
#[derive(Debug)]
pub enum Done {
    List {
        server: String,
        result: Result<Vec<Descriptor>, Failure>,
    },
    Poll {
        key: String,
        result: Result<PollPage, Failure>,
    },
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
                client,
                params,
                bound,
            } => Done::Poll {
                result: call(&client, wire::method::POLL, params, bound).and_then(|v| {
                    serde_json::from_value::<PollPage>(v)
                        .map_err(|e| Failure::Transport(format!("events/poll: {e}")))
                }),
                key,
            },
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
