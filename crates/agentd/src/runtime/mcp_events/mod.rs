// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Events (RFC 0045, SEP-3415): the bindings that bring a server's event
//! occurrences into declared streams.
//!
//! Each binding is discovered against its server's `events/list`, then polled
//! on the events worker pool; every occurrence becomes one stream event
//! (§5.4), and the binding's cursor never moves past an occurrence that is
//! not durably appended (§5.1 rule 2). A binding the server cannot honour is
//! refused, loudly, with its reason; one that fails in a way that may pass
//! backs off with jitter and keeps its cursor (§5.13).

pub mod bridge;
pub mod hook;
pub mod pool;
pub mod wire;

use super::reactor::Runtime;
use crate::config::settings::events::{Delivery, EventInto, Replay};
use crate::state::{Kind, now_ms};
use bridge::{Checked, Seen};
use pool::{Done, Failure, Job, Pool};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use wire::Descriptor;

/// The most occurrences one poll asks for.
const MAX_EVENTS: u32 = 50;
/// Polls in a row a binding makes while the server says `hasMore` and each
/// page held something new; past it, the next poll waits its interval.
const MAX_DRAIN: u32 = 10;
/// The floor under a server's `nextPollMs` [SEP L300].
const POLL_FLOOR_MS: u64 = 1_000;
/// The interval when a server names none.
const POLL_DEFAULT_MS: u64 = 30_000;
/// The longest a server's `nextPollMs` may hold a binding off: past it, a
/// server could park a binding for good.
const POLL_CEILING_MS: u64 = 15 * 60 * 1000;
/// How often a server's event types are listed again; the only way, beside
/// `list_changed`, a changed descriptor is seen (§5.3).
const RELIST_MS: u64 = 15 * 60 * 1000;
/// The backoff after a failure that may pass: from 1 s, doubling, to 5 min.
const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 5 * 60 * 1000;
/// The longest a server's `ResourceExhausted` holds a binding off.
const EXHAUSTED_MAX_MS: u64 = 15 * 60 * 1000;
/// How long one `events/*` request may take.
const REQUEST_BOUND: Duration = Duration::from_secs(30);
/// The least time between two reads of the instruction its occurrences
/// bring: a burst of them is one read.
const REREAD_FLOOR_MS: u64 = 1_000;

/// Where a binding is (§5.14).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum State {
    /// Waiting for its server's listing.
    Discovering,
    /// Polling.
    Active,
    /// Not polled while `cause` holds: `pressure`, `rate`, `store_degraded`.
    Paused(&'static str),
    /// Refused until the config, the connection or the server's listing
    /// changes: `reason` is one of §5.12's.
    Refused(&'static str),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::Discovering => "discovering",
            State::Active => "active",
            State::Paused(_) => "paused",
            State::Refused(_) => "refused",
        }
    }
}

/// Who holds a subscription. A binding lives while any owner does, and is
/// stopped — its records deleted — with the last (RFC 0045 §5.15).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Owner {
    /// The operator's `mcp.servers[].events`.
    Config,
    /// A suspended `wait {on: event, subscribe:}` step.
    Wait { run: String, step: String },
    /// An `events.subscribe` call: `context` names the caller, `run` its
    /// run when the caller is a workflow step.
    Tool {
        context: String,
        run: Option<String>,
        expires_at: u64,
    },
    /// The operator's `agent.instruction.mcp.events`: an occurrence re-reads
    /// the instruction rather than landing in a stream.
    Instruction,
}

impl Owner {
    /// Held at runtime, inside a grant — a wait or a tool call — rather than
    /// by what the settings declare.
    fn is_runtime(&self) -> bool {
        matches!(self, Owner::Wait { .. } | Owner::Tool { .. })
    }

    /// The same holder: a tool caller's context holds a subscription once,
    /// however often it renews it; any other owner is itself.
    fn same_holder(&self, other: &Owner) -> bool {
        match (self, other) {
            (Owner::Tool { context: a, .. }, Owner::Tool { context: b, .. }) => a == b,
            _ => self == other,
        }
    }

    /// How a log line names it.
    fn label(&self) -> Value {
        match self {
            Owner::Config => json!({"kind": "config"}),
            Owner::Wait { run, step } => json!({"kind": "wait", "run": run, "step": step}),
            Owner::Tool { context, run, .. } => {
                json!({"kind": "tool", "context": context, "run": run})
            }
            Owner::Instruction => json!({"kind": "instruction"}),
        }
    }
}

/// A runtime subscription made: where its occurrences land.
pub(crate) struct Subscribed {
    pub(crate) key: String,
    pub(crate) stream: String,
    pub(crate) subject: String,
}

/// Where the runtime-owned bindings are kept across a restart.
const RUNTIME_INDEX: &str = "_mcpev/runtime";
/// The most a runtime subscription's arguments may hold, serialized: they are
/// rendered from a run's inputs or chosen by a model, and kept in the index.
const MAX_ARGUMENT_BYTES: usize = 4096;
/// How often the loop looks for owners that are gone.
const SWEEP_EVERY_MS: u64 = 5_000;

/// Why a page stopped before its last occurrence.
enum Stop {
    /// The binding's `rate` ran dry.
    Rate,
    /// An append the stream refused (pressure), or a flush that failed.
    Refused(String),
}

/// What became of one occurrence offered to a binding (`events_take`).
enum Taken {
    Appended,
    /// Already in the dedup set.
    Duplicate,
    /// Never appendable — replay would bring the same bytes: `oversize` or
    /// `malformed`.
    Rejected(&'static str),
    /// The binding's `rate` is dry.
    Rate,
    /// The stream refused it (pressure).
    Refused(String),
}

/// One binding, live.
pub(crate) struct Binding {
    pub(crate) key: String,
    pub(crate) server: String,
    pub(crate) name: String,
    pub(crate) arguments: Map<String, Value>,
    delivery: Vec<Delivery>,
    /// The stream its occurrences land in; none when only the instruction
    /// follows it.
    into: Option<EventInto>,
    replay: Replay,
    /// The intake ceiling: `(burst, window seconds)`, and what is left of it.
    rate: Option<(u32, f64)>,
    tokens: f64,
    refilled_at: u64,
    pub(crate) state: State,
    mode: Option<Delivery>,
    cursor: Option<String>,
    /// The records were read (once per life).
    loaded: bool,
    seen: Seen,
    due_at: u64,
    in_flight: bool,
    drain: u32,
    failures: u32,
    payload_schema: Option<Value>,
    /// A payload failed its schema: list the server again.
    schema_relist: bool,
    /// That re-list was asked for already; it is asked for once per
    /// payload schema, not on every page that fails it.
    schema_relisted: bool,
    /// Bumped at every poll dispatched: an answer for an older one — a
    /// binding removed and declared again while its poll was out — is not
    /// this binding's.
    generation: u64,
    /// Who holds it: the config, waits, tool calls.
    pub(crate) owners: std::collections::BTreeSet<Owner>,
    /// Webhook: the lease to suggest (`ttl`).
    ttl_ms: u64,
    /// Webhook: the live subscription's secret, in memory only.
    secret: Option<String>,
    /// Webhook: subscribed, and deliveries are taken.
    armed: bool,
    /// Webhook: a subscription may exist on the server — an arm was sent,
    /// and no unsubscribe since.
    subscribed: bool,
    /// Webhook: when the lease is refreshed.
    refresh_at: u64,
    /// Webhook: deliveries refused (`429`/`503`) and not yet taken, by
    /// hashed `webhook-id`, with when each was first refused.
    refused: BTreeMap<String, u64>,
    /// Webhook: consecutive `CallbackEndpointError`s.
    hook_failures: u32,
    /// Webhook: the connection a reload replaced, unsubscribed on at the
    /// next arm.
    prior: Option<std::sync::Arc<crate::mcp::client::McpClient>>,
    /// Modes the server would not deliver in, passed over until a reload or
    /// a re-dial decides afresh.
    demoted: Vec<Delivery>,
    /// Its record is in the store.
    recorded: bool,
}

/// What a binding's config or its grant decides of it.
#[derive(Clone)]
struct Shape {
    delivery: Vec<Delivery>,
    into: Option<EventInto>,
    replay: Replay,
    rate: Option<(u32, f64)>,
    ttl_ms: u64,
}

impl Shape {
    fn of_binding(b: &crate::config::settings::events::EventBinding) -> Shape {
        Shape {
            delivery: b.delivery.clone(),
            into: Some(b.into.clone()),
            replay: b.replay,
            rate: b
                .rate
                .as_deref()
                .and_then(|r| crate::supervisor::tree::parse_rate(r).ok()),
            ttl_ms: b
                .ttl
                .as_ref()
                .map_or(hook::DEFAULT_TTL_MS, |d| d.0.as_millis() as u64),
        }
    }

    fn of_grant(g: &crate::config::settings::events::EventGrant) -> Shape {
        Shape {
            delivery: g.delivery.clone(),
            into: Some(g.into.clone()),
            replay: g.replay,
            rate: g
                .rate
                .as_deref()
                .and_then(|r| crate::supervisor::tree::parse_rate(r).ok()),
            ttl_ms: g
                .ttl
                .as_ref()
                .map_or(hook::DEFAULT_TTL_MS, |d| d.0.as_millis() as u64),
        }
    }

    /// What following the instruction asks: polled, kept nowhere, from
    /// "now" — the instruction is read afresh at every start, so nothing
    /// before it is worth catching up on.
    fn of_instruction() -> Shape {
        Shape {
            delivery: vec![Delivery::Poll],
            into: None,
            replay: Replay::None,
            rate: None,
            ttl_ms: hook::DEFAULT_TTL_MS,
        }
    }
}

impl Binding {
    fn new(
        key: String,
        server: &str,
        name: &str,
        arguments: Map<String, Value>,
        shape: Shape,
        now: u64,
    ) -> Binding {
        Binding {
            key,
            server: server.to_string(),
            name: name.to_string(),
            arguments,
            delivery: shape.delivery,
            into: shape.into,
            replay: shape.replay,
            rate: shape.rate,
            tokens: shape.rate.map_or(0.0, |(burst, _)| f64::from(burst)),
            refilled_at: now,
            state: State::Discovering,
            mode: None,
            cursor: None,
            loaded: false,
            seen: Seen::default(),
            due_at: now,
            in_flight: false,
            drain: 0,
            failures: 0,
            payload_schema: None,
            schema_relist: false,
            schema_relisted: false,
            generation: 0,
            owners: Default::default(),
            ttl_ms: shape.ttl_ms,
            secret: None,
            armed: false,
            subscribed: false,
            refresh_at: 0,
            refused: BTreeMap::new(),
            hook_failures: 0,
            prior: None,
            demoted: Vec::new(),
            recorded: false,
        }
    }

    /// Apply what the config or the grant now says, in place: a changed mode
    /// list is listed against again; the stream, catch-up and ceiling apply
    /// from the next poll.
    fn apply(&mut self, shape: Shape) {
        if self.delivery != shape.delivery {
            self.delivery = shape.delivery;
            self.mode = None;
            self.demoted.clear();
            self.state = State::Discovering;
        }
        self.into = shape.into;
        // Turned to `none`: from now, from the next arm or poll on. Already
        // `none`, the cursor this life reached stands — a reload is not
        // downtime.
        if matches!(shape.replay, Replay::None) && !matches!(self.replay, Replay::None) {
            self.cursor = None;
        }
        self.replay = shape.replay;
        self.ttl_ms = shape.ttl_ms;
        if self.rate != shape.rate {
            self.rate = shape.rate;
            self.tokens = shape.rate.map_or(0.0, |(burst, _)| f64::from(burst));
        }
    }

    fn cursor_key(&self) -> String {
        format!("_mcpev/{}/cursor", self.key)
    }

    fn seen_key(&self) -> String {
        format!("_mcpev/{}/seen", self.key)
    }

    /// The tokens the intake ceiling holds now.
    fn refill(&mut self, now: u64) {
        if let Some((burst, per_s)) = self.rate {
            let rate = f64::from(burst) / per_s.max(0.001);
            let elapsed = now.saturating_sub(self.refilled_at) as f64 / 1000.0;
            self.tokens = (self.tokens + elapsed * rate).min(f64::from(burst));
            self.refilled_at = now;
        }
    }

    /// When the ceiling next holds a whole token.
    fn refill_at(&self, now: u64) -> u64 {
        match self.rate {
            Some((burst, per_s)) => {
                let rate = f64::from(burst) / per_s.max(0.001);
                now + ((1.0 - self.tokens).max(0.0) / rate * 1000.0).ceil() as u64
            }
            None => now,
        }
    }
}

/// One server's listing.
#[derive(Default)]
struct Listing {
    descriptors: Option<BTreeMap<String, Descriptor>>,
    in_flight: bool,
    due_at: u64,
    failures: u32,
    /// Why the server cannot be used for Events at all, while it holds: the
    /// reason, and what to say.
    refused: Option<(&'static str, String)>,
}

/// The loop's MCP Events state.
#[derive(Default)]
pub(crate) struct Events {
    pub(crate) bindings: BTreeMap<String, Binding>,
    listings: BTreeMap<String, Listing>,
    pool: Option<Pool>,
    /// The next poll generation; never reused within a life.
    next_generation: u64,
    /// When the owners were last looked over.
    swept_at: u64,
    /// The webhook bindings' routes, shared with the webhook listener.
    pub(crate) hooks: hook::Routes,
    /// An occurrence of an event type the instruction follows came in, and
    /// the instruction is to be read again: `(server, event type)` of the
    /// latest.
    reread: Option<(String, String)>,
    /// When the instruction was last read again for one.
    reread_at: u64,
}

impl Events {
    /// `(active, paused, refused)` bindings, for the status counters.
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        let mut c = (0, 0, 0);
        for b in self.bindings.values() {
            match b.state {
                State::Active => c.0 += 1,
                State::Paused(_) => c.1 += 1,
                State::Refused(_) => c.2 += 1,
                State::Discovering => {}
            }
        }
        c
    }
}

/// What a supervisor connection adds to its listen for MCP Events: when the
/// server declares the extension with `listChanged: true` (either spelling,
/// Q2), `eventsListChanged` in the filter, and `notifications/events/
/// list_changed` taken off the stream for the loop [SEP L192-208].
pub(crate) fn listen_extension() -> ::mcp::rmcp_client::ListenExtensionFn {
    std::sync::Arc::new(|caps: &Value| {
        let declared = wire::declared(caps)?;
        (declared.get("listChanged") == Some(&Value::Bool(true))).then(|| {
            let mut members = Map::new();
            members.insert(wire::LIST_CHANGED_FILTER.into(), Value::Bool(true));
            ::mcp::rmcp_transport::ListenExtension {
                members,
                methods: vec![wire::method::LIST_CHANGED.to_string()],
            }
        })
    })
}

/// `base` with up to ±10% jitter: polling clients MUST jitter (2026-07-28
/// caching), so many bindings on one server do not poll in step.
fn jitter(base: u64) -> u64 {
    let mut b = [0u8; 8];
    if crate::sec::random::fill(&mut b).is_err() {
        return base;
    }
    let spread = base / 10;
    if spread == 0 {
        return base;
    }
    let offset = u64::from_le_bytes(b) % spread.saturating_mul(2).saturating_add(1);
    (base - spread).saturating_add(offset)
}

/// The backoff after `failures` failures in a row, capped at `max`.
fn backoff(failures: u32, max: u64) -> u64 {
    jitter(
        BACKOFF_MIN_MS
            .saturating_mul(1u64 << failures.min(20))
            .min(max),
    )
}

/// Strip control characters and cap at 200 bytes: server text in a log line
/// must not be able to drive an operator's terminal (§5.12).
fn tame(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.len() > 200 {
        let mut cut = 200;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

impl Runtime {
    /// Bring the live bindings in line with the settings: every binding the
    /// config declares, keyed by its identity, and every runtime
    /// subscription its grant still allows. A binding that is unchanged keeps
    /// its state; one no owner holds any more stops and its records go; a
    /// changed `into`, `replay` or `rate` applies in place. `redialed` names
    /// the servers whose connection was replaced, whose event types are
    /// listed again. Called at boot (which restores the runtime-owned
    /// bindings first), on every reload and after a re-dial (§5.7).
    pub(crate) fn events_sync(&mut self, reason: &str, redialed: &[String]) {
        let now = now_ms();
        if reason == "boot" {
            self.events_restore_runtime(now);
        }
        let mut wanted = std::collections::BTreeSet::new();
        let mut config_moved: Vec<String> = Vec::new();
        let declared: Vec<(String, crate::config::settings::events::EventBinding)> = self
            .settings
            .mcp
            .servers
            .iter()
            .flat_map(|s| s.events.iter().map(move |b| (s.name.clone(), b.clone())))
            .collect();
        for (server, b) in declared {
            let key = bridge::binding_hash(&server, &b.name, &b.arguments);
            wanted.insert(key.clone());
            match self.mcp_events.bindings.get_mut(&key) {
                // The same subscription: what changed applies in place.
                Some(live) => {
                    live.owners.insert(Owner::Config);
                    let was = live.into.clone();
                    live.apply(Shape::of_binding(&b));
                    if was.is_some() && was != live.into {
                        config_moved.push(key.clone());
                    }
                }
                None => {
                    let mut fresh = Binding::new(
                        key.clone(),
                        &server,
                        &b.name,
                        b.arguments.clone(),
                        Shape::of_binding(&b),
                        now,
                    );
                    fresh.owners.insert(Owner::Config);
                    self.mcp_events.bindings.insert(key, fresh);
                }
            }
        }
        // The config's hold where the settings no longer declare it; and a
        // runtime subscription follows its grant as it is now — gone with it,
        // gone when the grant no longer allows it (a `fixed` value changed,
        // the tools withdrawn), a tool's hold no longer than the lifetime,
        // its fields applied in place where no binding of the config holds
        // it.
        let grants = self.events_grants();
        let mut dropped: Vec<(String, Owner, &'static str)> = Vec::new();
        let mut renewed: Vec<(String, Owner, Owner)> = Vec::new();
        let mut moved: Vec<String> = Vec::new();
        for b in self.mcp_events.bindings.values_mut() {
            if !wanted.contains(&b.key) {
                b.owners.remove(&Owner::Config);
            }
            let runtime: Vec<Owner> = b
                .owners
                .iter()
                .filter(|o| o.is_runtime())
                .cloned()
                .collect();
            if runtime.is_empty() {
                continue;
            }
            let Some(g) = grants.get(&(b.server.clone(), b.name.clone())) else {
                dropped.extend(
                    runtime
                        .into_iter()
                        .map(|o| (b.key.clone(), o, "grant_removed")),
                );
                continue;
            };
            if g.fixed.iter().any(|(k, v)| b.arguments.get(k) != Some(v)) {
                dropped.extend(
                    runtime
                        .into_iter()
                        .map(|o| (b.key.clone(), o, "grant_narrowed")),
                );
                continue;
            }
            let lifetime = g
                .lifetime
                .as_ref()
                .map_or(DEFAULT_LIFETIME_MS, |d| d.0.as_millis() as u64);
            for o in runtime {
                if let Owner::Tool {
                    context,
                    run,
                    expires_at,
                } = &o
                {
                    if !g.tools {
                        dropped.push((b.key.clone(), o.clone(), "grant_narrowed"));
                    } else if *expires_at > now.saturating_add(lifetime) {
                        let shorter = Owner::Tool {
                            context: context.clone(),
                            run: run.clone(),
                            expires_at: now.saturating_add(lifetime),
                        };
                        renewed.push((b.key.clone(), o.clone(), shorter));
                    }
                }
            }
            if !b.owners.contains(&Owner::Config) {
                let was = b.into.clone();
                b.apply(Shape::of_grant(g));
                if was.is_some() && was != b.into {
                    moved.push(b.key.clone());
                }
            }
        }
        for (key, old, new) in renewed {
            if let Some(b) = self.mcp_events.bindings.get_mut(&key) {
                b.owners.remove(&old);
                b.owners.insert(new);
            }
        }
        for (key, owner, why) in dropped {
            self.events_release(&key, &owner, why);
        }
        for key in moved.into_iter().chain(config_moved) {
            self.events_follow_stream(&key);
        }
        self.events_follow_instruction(now);
        self.events_enforce_caps();
        if reason == "boot" {
            self.events_reconcile_waits();
        }
        let ownerless: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.owners.is_empty())
            .map(|b| b.key.clone())
            .collect();
        for key in ownerless {
            self.events_remove(&key, reason);
        }
        self.events_save_runtime();
        // Servers neither a binding nor a grant names any more, and the ones
        // dialed afresh.
        let servers = self.events_servers();
        self.mcp_events
            .listings
            .retain(|s, _| servers.contains(s) && !redialed.contains(s));
        // A reload or a re-dial is when a refusal is looked at again (a new
        // credential, a server changed); a listing alone clears only what it
        // decided (`events_evaluate`).
        for b in self.mcp_events.bindings.values_mut() {
            if (redialed.contains(&b.server) || matches!(b.state, State::Refused(_)))
                && !b.in_flight
            {
                b.state = State::Discovering;
                b.mode = None;
                b.demoted.clear();
            }
        }
        if self.mcp_events.bindings.is_empty() {
            crate::obs::metrics::set_event_subscriptions(Vec::new());
        }
        if !servers.is_empty() && self.mcp_events.pool.is_none() {
            self.mcp_events.pool = Some(Pool::start(self.events_tx.clone()));
        }
        // A webhook binding being decided again (its modes changed, a refusal
        // looked at again, a re-dial) takes no deliveries until it is armed
        // afresh — an arm always unsubscribes first — or, deciding on another
        // mode, lets its subscription go (`events_evaluate`). Then every
        // binding that may deliver by webhook has its route.
        let reset: Vec<String> = self
            .mcp_events
            .bindings
            .values_mut()
            .filter(|b| b.mode.is_none() && (b.armed || b.secret.is_some()))
            .map(|b| {
                b.armed = false;
                b.secret = None;
                b.key.clone()
            })
            .collect();
        for key in reset {
            self.mcp_events
                .hooks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key)
                .and_modify(|r| {
                    r.armed = false;
                    r.secret = None;
                    r.pending = None;
                });
        }
        self.events_sync_routes();
        self.events_hold_pending();
    }

    /// A binding added (or whose modes changed) on a server already listed
    /// is held against that listing now, not at the next re-list — and one
    /// on a server already refused is refused with it, not left waiting.
    fn events_hold_pending(&mut self) {
        let pending: Vec<(String, Option<(&'static str, String)>)> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.state == State::Discovering)
            .filter_map(|b| {
                let l = self.mcp_events.listings.get(&b.server)?;
                match &l.refused {
                    Some(r) => Some((b.key.clone(), Some(r.clone()))),
                    None => l.descriptors.is_some().then(|| (b.key.clone(), None)),
                }
            })
            .collect();
        for (key, refused) in pending {
            match refused {
                Some((reason, detail)) => self.events_refuse(&key, reason, &detail),
                None => self.events_evaluate(&key),
            }
        }
    }

    /// The instruction's own event types (`agent.instruction.mcp.events`),
    /// held on the server that served it while it is the running
    /// instruction. A binding the config or a grant holds already is
    /// followed as it is; one only the instruction holds is polled and kept
    /// in no stream.
    fn events_follow_instruction(&mut self, now: u64) {
        let mut wanted = std::collections::BTreeSet::new();
        let source = self.settings.agent.instruction_spec.mcp.clone();
        if let Some(m) = &source
            && self.instruction.source == "resource"
            && self.instruction.uri.as_deref() == Some(m.resource())
            && let Some(server) = self.instruction.server.clone()
        {
            for e in m.events() {
                let key = bridge::binding_hash(&server, &e.name, &e.arguments);
                wanted.insert(key.clone());
                self.mcp_events
                    .bindings
                    .entry(key.clone())
                    .or_insert_with(|| {
                        Binding::new(
                            key,
                            &server,
                            &e.name,
                            e.arguments.clone(),
                            Shape::of_instruction(),
                            now,
                        )
                    })
                    .owners
                    .insert(Owner::Instruction);
            }
        }
        let mut keys = Vec::new();
        for b in self.mcp_events.bindings.values_mut() {
            if !wanted.contains(&b.key) {
                b.owners.remove(&Owner::Instruction);
            }
            keys.push(b.key.clone());
        }
        for key in keys {
            self.events_fall_back_to_instruction(&key);
        }
    }

    /// When only the instruction holds `key` — the config's binding and the
    /// grants' holds gone — it takes the instruction's shape: polled, kept in
    /// no stream. It keeps its cursor, so an edit since its last poll still
    /// brings a read; a webhook subscription is let go when it is next held
    /// against its listing (`events_evaluate`). Whether it changed.
    fn events_fall_back_to_instruction(&mut self, key: &str) -> bool {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return false;
        };
        if b.into.is_none()
            || b.owners.is_empty()
            || b.owners.iter().any(|o| *o != Owner::Instruction)
        {
            return false;
        }
        let cursor = b.cursor.clone();
        b.apply(Shape::of_instruction());
        b.cursor = cursor;
        true
    }

    /// The running instruction moved (`instruction.subscribe` named another
    /// source): its event types follow it — let go where it was, held where
    /// it is — without the rest of a sync.
    pub(crate) fn events_instruction_moved(&mut self) {
        self.events_follow_instruction(now_ms());
        let ownerless: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.owners.is_empty())
            .map(|b| b.key.clone())
            .collect();
        for key in ownerless {
            self.events_remove(&key, "instruction_moved");
        }
        if !self.events_servers().is_empty() && self.mcp_events.pool.is_none() {
            self.mcp_events.pool = Some(Pool::start(self.events_tx.clone()));
        }
        self.events_sync_routes();
        self.events_hold_pending();
    }

    /// Read the instruction again for the occurrences that came in, at most
    /// once per [`REREAD_FLOOR_MS`]; what is new is `instruction.updated`.
    fn events_reread_instruction(&mut self, now: u64) {
        if now < self.mcp_events.reread_at.saturating_add(REREAD_FLOOR_MS) {
            return;
        }
        let Some((server, event)) = self.mcp_events.reread.take() else {
            return;
        };
        self.mcp_events.reread_at = now;
        self.log.info(
            "instruction.event",
            json!({"server": server, "event_type": event}),
        );
        if let Err(e) = self.reread_instruction() {
            self.log.warn(
                "instruction.reread.fail",
                json!({"server": server, "uri": self.instruction.uri, "reason": "event", "err": e}),
            );
        }
    }

    /// Every pass of the loop: list what needs listing, poll what is due.
    pub(crate) fn events_tick(&mut self) {
        if self.mcp_events.pool.is_none() {
            return; // nothing declared, nothing subscribed
        }
        let now = now_ms();
        self.events_sweep_owners(now);
        self.events_list_due(now);
        self.events_poll_due(now);
        self.events_hook_due(now);
        self.events_reread_instruction(now);
        let mut active: BTreeMap<String, u64> = BTreeMap::new();
        for b in self.mcp_events.bindings.values() {
            if b.state == State::Active
                && let Some(m) = b.mode
            {
                *active.entry(m.as_str().to_string()).or_default() += 1;
            }
        }
        crate::obs::metrics::set_event_subscriptions(active.into_iter().collect());
    }

    /// Ask each server with a binding or a grant for its event types, when
    /// it has none yet or its re-list is due — once the connection shows it
    /// can be asked.
    fn events_list_due(&mut self, now: u64) {
        for server in self.events_servers() {
            let schema_relist = self
                .mcp_events
                .bindings
                .values()
                .any(|b| b.server == server && b.schema_relist);
            let listing = self.mcp_events.listings.entry(server.clone()).or_default();
            if listing.in_flight || (now < listing.due_at && !schema_relist) {
                continue;
            }
            let Some(client) = self.mcp.get(&server).cloned() else {
                continue; // not connected: a reload or a re-dial brings it
            };
            // The extension is defined only from 2026-07-28 [SEP L37].
            let revision = client.protocol_version();
            let refusal = if revision.as_deref() != Some(wire::REVISION) {
                Some("revision")
            } else {
                let caps = json!({
                    "extensions": client.capability("extensions"),
                    "events": client.capability("events"),
                });
                wire::declared(&caps).is_none().then_some("no_capability")
            };
            if let Some(reason) = refusal {
                let detail = match reason {
                    "revision" => format!(
                        "the connection reached {}; MCP Events is defined only from {} — no events/* request is sent",
                        revision.as_deref().unwrap_or("no revision"),
                        wire::REVISION
                    ),
                    _ => "the server does not declare the io.modelcontextprotocol/events extension — no events/* request is sent".to_string(),
                };
                let listing = self.mcp_events.listings.entry(server.clone()).or_default();
                listing.refused = Some((reason, detail.clone()));
                listing.due_at = u64::MAX;
                self.events_refuse_server(&server, reason, &detail);
                continue;
            }
            let listing = self.mcp_events.listings.entry(server.clone()).or_default();
            listing.refused = None;
            listing.in_flight = true;
            for b in self.mcp_events.bindings.values_mut() {
                if b.server == server {
                    b.schema_relist = false;
                }
            }
            if let Some(pool) = &self.mcp_events.pool {
                pool.submit(Job::List {
                    server,
                    client,
                    bound: REQUEST_BOUND,
                });
            }
        }
    }

    /// Refuse every binding on `server` for `reason`.
    fn events_refuse_server(&mut self, server: &str, reason: &'static str, detail: &str) {
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.server == server)
            .map(|b| b.key.clone())
            .collect();
        for key in keys {
            self.events_refuse(&key, reason, detail);
        }
    }

    /// Refuse one binding, said once per refusal.
    fn events_refuse(&mut self, key: &str, reason: &'static str, detail: &str) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.state == State::Refused(reason) {
            return;
        }
        self.events_hook_stop(key, false);
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.state = State::Refused(reason);
        b.mode = None;
        self.log.error(
            "mcp.events.refused",
            json!({"server": b.server, "event_type": b.name, "reason": reason, "detail": tame(detail)}),
        );
        // A wait suspended on it would wait for nothing: it fails, and lets
        // go. A tool's hold stays, its state `refused`, until the listing,
        // the connection or the config changes.
        let waits: Vec<Owner> = b
            .owners
            .iter()
            .filter(|o| matches!(o, Owner::Wait { .. }))
            .cloned()
            .collect();
        for o in waits {
            self.events_release(key, &o, "refused");
        }
    }

    /// Dispatch every poll that is due and not held back.
    fn events_poll_due(&mut self, now: u64) {
        let pressure = self.pressure.refusal(false);
        let degraded = self.durable.is_degraded();
        let keys: Vec<String> = self.mcp_events.bindings.keys().cloned().collect();
        for key in keys {
            let held = {
                let Some(b) = self.mcp_events.bindings.get_mut(&key) else {
                    continue;
                };
                if !matches!(b.state, State::Active | State::Paused(_))
                    || b.in_flight
                    || b.mode != Some(Delivery::Poll)
                {
                    continue;
                }
                if now < b.due_at {
                    continue;
                }
                b.refill(now);
                if pressure.is_some() {
                    Some("pressure")
                } else if degraded {
                    Some("store_degraded")
                } else if b.rate.is_some() && b.tokens < 1.0 {
                    b.due_at = b.refill_at(now);
                    Some("rate")
                } else {
                    None
                }
            };
            // The records are read before the first poll: a poll from a
            // cursor the store could not give back would move past whatever
            // it held, with nothing said (§5.1 rule 2).
            let unread = self
                .mcp_events
                .bindings
                .get(&key)
                .is_some_and(|b| !b.loaded);
            if held.is_none() && unread && self.events_load_records(&key).is_err() {
                self.events_pause(&key, "store_degraded");
                continue;
            }
            match held {
                Some(cause) => self.events_pause(&key, cause),
                None => self.events_dispatch_poll(&key, now),
            }
        }
    }

    fn events_pause(&mut self, key: &str, cause: &'static str) {
        self.events_pause_for(key, cause, None);
    }

    /// Pause a binding for `cause`, said once per pause; held at its cursor.
    fn events_pause_for(&mut self, key: &str, cause: &'static str, detail: Option<&str>) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.state != State::Paused(cause) {
            b.state = State::Paused(cause);
            let mut line = json!({"server": b.server, "event_type": b.name, "cause": cause});
            if let Some(d) = detail {
                line["detail"] = json!(tame(d));
            }
            self.log.warn("mcp.events.paused", line);
        }
        if cause != "rate" {
            b.failures += 1;
            b.due_at = now_ms() + backoff(b.failures, BACKOFF_MAX_MS).min(30_000);
        }
    }

    fn events_dispatch_poll(&mut self, key: &str, now: u64) {
        let Some(client) = self
            .mcp_events
            .bindings
            .get(key)
            .and_then(|b| self.mcp.get(&b.server).cloned())
        else {
            return;
        };
        // The service's `rate` models the upstream's limit, which tool calls
        // and `events/*` both spend: a dry bucket defers the poll.
        let server = self.mcp_events.bindings[key].server.clone();
        if crate::mcp::pace::take(&server).is_err() {
            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                b.due_at = now + 1_000;
            }
            return;
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if let State::Paused(cause) = b.state {
            b.state = State::Active;
            self.log.info(
                "mcp.events.resumed",
                json!({"server": b.server, "event_type": b.name, "cause": cause}),
            );
        }
        let max_events = match b.rate {
            Some(_) => (b.tokens.floor() as u32).clamp(1, MAX_EVENTS),
            None => MAX_EVENTS,
        };
        let max_age = match b.replay {
            Replay::Within(d) => Some(d.as_millis() as u64),
            Replay::None => None,
        };
        let params = wire::poll_params(
            &b.name,
            &b.arguments,
            b.cursor.as_deref(),
            max_age,
            max_events,
        );
        b.in_flight = true;
        self.mcp_events.next_generation += 1;
        let generation = self.mcp_events.next_generation;
        if let Some(b) = self.mcp_events.bindings.get_mut(key) {
            b.generation = generation;
        }
        if let Some(pool) = &self.mcp_events.pool {
            pool.submit(Job::Poll {
                key: key.to_string(),
                generation,
                client,
                params,
                bound: REQUEST_BOUND,
            });
        }
    }

    /// The server said its event types changed: list them again now, rather
    /// than at the next 15-minute re-list.
    pub(crate) fn events_list_changed(&mut self, server: &str) {
        if let Some(l) = self.mcp_events.listings.get_mut(server)
            && l.refused.is_none()
        {
            l.due_at = 0;
            self.log
                .info("mcp.events.list_changed", json!({"server": server}));
        }
    }

    /// An answer from the pool.
    pub(crate) fn on_events_done(&mut self, done: Done) {
        let outcome = |failed: Option<&Failure>| match failed {
            None => "ok",
            Some(Failure::Rpc(_)) => "refused",
            Some(_) => "failed",
        };
        match done {
            Done::List { server, result } => {
                crate::obs::metrics::record_mcp_events_request(
                    &server,
                    wire::method::LIST,
                    outcome(result.as_ref().err()),
                );
                self.events_listed(&server, result)
            }
            Done::Poll {
                key,
                generation,
                result,
            } => {
                let Some(b) = self.mcp_events.bindings.get(&key) else {
                    return; // removed while its poll was out
                };
                crate::obs::metrics::record_mcp_events_request(
                    &b.server,
                    wire::method::POLL,
                    outcome(result.as_ref().err()),
                );
                // An answer for a poll this binding did not send — it was
                // removed and declared again meanwhile — is dropped: applied,
                // it could move the cursor past what was never appended.
                if b.generation != generation {
                    return;
                }
                self.events_polled(&key, result)
            }
            other => self.events_hook_done(other),
        }
    }

    fn events_listed(&mut self, server: &str, result: Result<Vec<Descriptor>, Failure>) {
        let now = now_ms();
        let Some(listing) = self.mcp_events.listings.get_mut(server) else {
            return; // no binding names the server any more
        };
        listing.in_flight = false;
        let descriptors = match result {
            Ok(d) => d,
            Err(Failure::Rpc(e)) if e.code == wire::code::METHOD_NOT_FOUND => {
                listing.due_at = now + jitter(RELIST_MS);
                self.events_refuse_server(
                    server,
                    "method_not_found",
                    &format!("events/list: {} — the server (or a gateway in front of it) does not serve MCP Events", e.message),
                );
                return;
            }
            Err(Failure::TooLarge) => {
                listing.due_at = now + jitter(RELIST_MS);
                self.events_refuse_server(server, "list_too_large", &Failure::TooLarge.to_string());
                return;
            }
            Err(e) => {
                listing.failures += 1;
                let wait = backoff(listing.failures, BACKOFF_MAX_MS);
                listing.due_at = now + wait;
                self.log.warn(
                    "mcp.events.list.fail",
                    json!({"server": server, "err": tame(&e.to_string()), "retry_ms": wait}),
                );
                return;
            }
        };
        listing.failures = 0;
        listing.due_at = now + jitter(RELIST_MS);
        let types = descriptors.len();
        listing.descriptors = Some(
            descriptors
                .into_iter()
                .map(|d| (d.name.clone(), d))
                .collect(),
        );
        self.log.info(
            "mcp.events.listed",
            json!({"server": server, "types": types}),
        );
        // A listing clears what a listing decides; what the server said to a
        // poll — not this credential, not this mode — waits for a reload or a
        // re-dial.
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.server == server)
            .filter(|b| !matches!(b.state, State::Refused("forbidden" | "not_supported")))
            .map(|b| b.key.clone())
            .collect();
        for key in keys {
            self.events_evaluate(&key);
        }
    }

    /// Hold one binding against its server's listing (§5.3 step 4): the type
    /// listed, a mode both sides accept, arguments its `inputSchema` takes.
    /// Re-run at every listing, so a refusal clears once the server changes.
    fn events_evaluate(&mut self, key: &str) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let Some(descriptors) = self
            .mcp_events
            .listings
            .get(&b.server)
            .and_then(|l| l.descriptors.as_ref())
        else {
            return;
        };
        let Some(d) = descriptors.get(&b.name).cloned() else {
            return self.events_refuse(
                key,
                "unknown_event",
                "the server's events/list does not name this event type",
            );
        };
        // The first mode the operator listed that the server offers (push
        // waits on the SEP, P4).
        let mode = b
            .delivery
            .iter()
            .copied()
            .filter(|m| !b.demoted.contains(m))
            .find(|m| d.delivery.iter().any(|x| x == m.as_str()));
        let Some(mode) = mode else {
            let detail = format!(
                "the operator lists {:?}, the server offers {:?}",
                b.delivery.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                d.delivery
            );
            return self.events_refuse(key, "no_common_mode", &detail);
        };
        if let Some(schema) = &d.input_schema
            && let Err(errs) =
                crate::jsonschema::validate(schema, &Value::Object(b.arguments.clone()))
        {
            let detail = crate::jsonschema::explain(&errs);
            return self.events_refuse(key, "invalid_arguments", &detail);
        }
        let was = b.state.clone();
        if !b.loaded {
            // A read that fails is retried before the first poll.
            let _ = self.events_load_records(key);
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.payload_schema != d.payload_schema {
            b.schema_relisted = false;
        }
        b.payload_schema = d.payload_schema.clone();
        let left_webhook = mode != Delivery::Webhook && (b.subscribed || b.prior.is_some());
        b.mode = Some(mode);
        if left_webhook {
            self.events_hook_stop(key, false);
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if matches!(was, State::Discovering | State::Refused(_)) {
            b.state = State::Active;
            // Re-armed after a failure (a type the server said it lost, a
            // schema it changed): the backoff the failure earned still holds.
            b.due_at = if b.failures > 0 {
                now + backoff(b.failures, BACKOFF_MAX_MS)
            } else {
                now
            };
            // A webhook binding is armed when its subscribe answers
            // (`hook::events_armed`).
            b.armed = false;
            if mode == Delivery::Poll {
                self.log.info(
                    "mcp.events.armed",
                    json!({"server": b.server, "event_type": b.name, "mode": mode.as_str(),
                           "cursor_present": b.cursor.is_some()}),
                );
            }
        }
    }

    /// Read a binding's records, once per life: its cursor and its dedup set.
    /// Only a record that is not there starts the binding from "now" [SEP
    /// L690]; a read that failed is an error, and the binding is not polled
    /// until a read succeeds.
    fn events_load_records(&mut self, key: &str) -> Result<(), ()> {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return Ok(());
        };
        let record = self.durable.get(Kind::Memory, &b.cursor_key());
        let seen = self.durable.get(Kind::Memory, &b.seen_key());
        let (Ok(record), Ok(seen)) = (record, seen) else {
            return Err(());
        };
        b.loaded = true;
        b.recorded = record.is_some();
        if let Some(seen) = seen {
            b.seen = Seen::from_json(&seen.state["entries"]);
        }
        let Some(record) = record.map(|e| e.state) else {
            return Ok(()); // a new binding starts from "now"
        };
        b.cursor = match b.replay {
            // `replay: none` always re-arms from now.
            Replay::None => None,
            Replay::Within(_) => record["cursor"].as_str().map(str::to_string),
        };
        if b.cursor.is_none() && matches!(b.replay, Replay::Within(_)) {
            // A type without replay, re-armed after downtime: whatever
            // happened in between is gone, and said so.
            self.log.warn(
                "mcp.events.gap",
                json!({"server": b.server, "event_type": b.name, "cause": "no_replay"}),
            );
            crate::obs::metrics::record_mcp_event_gap(&b.server, &b.name, "no_replay");
        }
        Ok(())
    }

    fn events_polled(&mut self, key: &str, result: Result<wire::PollPage, Failure>) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return; // removed while its poll was out
        };
        b.in_flight = false;
        if b.state != State::Active {
            return; // refused or paused while its poll was out
        }
        let page = match result {
            Ok(p) => p,
            Err(e) => return self.events_poll_failed(key, e),
        };
        if page
            .cursor
            .as_ref()
            .is_some_and(|c| c.len() > bridge::MAX_CURSOR_BYTES)
        {
            return self.events_poll_failed(
                key,
                Failure::Transport(format!(
                    "events/poll: a cursor over {} bytes",
                    bridge::MAX_CURSOR_BYTES
                )),
            );
        }
        let mut appended = 0u64;
        let mut fresh = 0u64;
        // Why the page stopped short, if it did: the cursor then holds, and
        // what was appended is in the dedup set, so the poll that resumes
        // appends the rest and nothing twice.
        let mut stopped: Option<Stop> = None;
        for raw in &page.events {
            match self.events_take(key, raw, "poll", now) {
                Taken::Appended => {
                    appended += 1;
                    fresh += 1;
                }
                Taken::Duplicate | Taken::Rejected(_) => {}
                Taken::Rate => {
                    stopped = Some(Stop::Rate);
                    break;
                }
                Taken::Refused(e) => {
                    fresh += 1;
                    stopped = Some(Stop::Refused(e));
                    break;
                }
            }
        }
        // Nothing moves past an occurrence that is not durably kept: whenever
        // this pass appended or is about to move the cursor, the stream is
        // flushed first (§5.1 rule 2) — a page of duplicates after a flush
        // that failed included.
        let moving = stopped.is_none()
            && self
                .mcp_events
                .bindings
                .get(key)
                .is_some_and(|b| b.cursor != page.cursor);
        let flushed = if appended > 0 || moving {
            self.durable.flush(true).map(|_| ())
        } else {
            Ok(())
        };
        if let Err(e) = &flushed {
            stopped = Some(Stop::Refused(format!("the stream head was not saved: {e}")));
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.seen.expire(now);
        if appended > 0 && flushed.is_ok() {
            let _ = self.durable.put(
                Kind::Memory,
                &b.seen_key(),
                json!({"entries": b.seen.to_json()}),
                None,
            );
        }
        match stopped {
            Some(Stop::Rate) => {
                b.due_at = b.refill_at(now);
                return self.events_pause_for(key, "rate", None);
            }
            Some(Stop::Refused(why)) => {
                let cause = if self.pressure.refusal(false).is_some() {
                    "pressure"
                } else {
                    "store_degraded"
                };
                return self.events_pause_for(key, cause, Some(&why));
            }
            None => {}
        }
        b.failures = 0;
        if page.truncated {
            crate::obs::metrics::record_mcp_event_gap(&b.server, &b.name, "truncated");
            self.log.warn(
                "mcp.events.gap",
                json!({"server": b.server, "event_type": b.name, "cause": "truncated"}),
            );
        }
        // The record is written when the cursor moves, and once at first —
        // so a type without replay, whose cursor never moves, still has one,
        // and a re-arm after downtime says what it could not recover.
        let write = page.cursor != b.cursor || !b.recorded;
        b.cursor = page.cursor.clone();
        // Drain a backlog at once while each page holds something new — but
        // a bounded number of times: a server that always says `hasMore` is
        // not a reason to poll in a loop (§5.6.1).
        b.due_at = if page.has_more && fresh > 0 && b.drain < MAX_DRAIN {
            b.drain += 1;
            now
        } else {
            b.drain = 0;
            now + jitter(
                page.next_poll_ms
                    .unwrap_or(POLL_DEFAULT_MS)
                    .clamp(POLL_FLOOR_MS, POLL_CEILING_MS),
            )
        };
        if write {
            self.events_record(key);
        }
    }

    /// Write a binding's record: its mode, its cursor, its state.
    fn events_record(&mut self, key: &str) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        let record = json!({"mode": b.mode.map(Delivery::as_str), "cursor": b.cursor,
                            "state": b.state.name(), "since": now_ms()});
        if self
            .durable
            .put(Kind::Memory, &b.cursor_key(), record, None)
            .is_ok()
        {
            b.recorded = true;
        }
    }

    /// Offer one occurrence to a binding, whichever mode delivered it: checked
    /// (§5.4), deduplicated, held to the binding's `rate`, appended. What was
    /// appended is in the dedup set and has spent a token; the caller flushes
    /// the stream before anything says it is kept.
    fn events_take(&mut self, key: &str, raw: &Value, mode: &'static str, now: u64) -> Taken {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return Taken::Rejected("malformed");
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        let follows = b.owners.contains(&Owner::Instruction);
        // Where it lands: the stream of whatever else holds it — none when
        // only the instruction does.
        let into = b
            .into
            .clone()
            .filter(|_| b.owners.iter().any(|o| *o != Owner::Instruction));
        let max_value_bytes = self.settings.store.max_value_bytes.map(|n| n as usize);
        let checked = bridge::check(
            &server,
            &name,
            key,
            raw,
            b.payload_schema.as_ref(),
            max_value_bytes,
        );
        let (id, event_id, data, schema_ok) = match checked {
            Checked::Rejected { cause, event_id } => {
                crate::obs::metrics::record_mcp_event(&server, &name, mode, "rejected");
                self.log.warn(
                    "mcp.events.rejected",
                    json!({"server": server, "event_type": name, "event_id": event_id.as_deref().map(tame), "cause": cause}),
                );
                return Taken::Rejected(cause);
            }
            Checked::Append {
                id,
                event_id,
                data,
                schema_ok,
            } => (id, event_id, data, schema_ok),
        };
        let seen_key = bridge::seen_key(&event_id);
        if b.seen.contains(&seen_key) {
            crate::obs::metrics::record_mcp_event(&server, &name, mode, "duplicate");
            return Taken::Duplicate;
        }
        // The intake ceiling holds whatever the server sends: one that ignored
        // `maxEvents`, or delivers faster than the ceiling, is stopped here.
        if b.rate.is_some() && b.tokens < 1.0 {
            return Taken::Rate;
        }
        let occurred_at = data["occurred_at"].as_str().and_then(bridge::timestamp_ms);
        if follows {
            self.mcp_events.reread = Some((server.clone(), name.clone()));
        }
        let Some(EventInto { stream, subject }) = into else {
            // Followed, kept nowhere: the instruction's read is what it
            // brings.
            crate::obs::metrics::record_mcp_event(&server, &name, mode, "followed");
            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                let evicted = b.seen.insert(seen_key, now);
                crate::obs::metrics::record_mcp_events_dedup_evicted(&server, &name, evicted);
            }
            return Taken::Appended;
        };
        match self.append(&stream, &subject, None, data, &id, &bridge::source(&server)) {
            Ok(_) => {
                crate::obs::metrics::record_mcp_event(
                    &server,
                    &name,
                    mode,
                    if schema_ok {
                        "appended"
                    } else {
                        "schema_mismatch"
                    },
                );
                if let Some(at) = occurred_at {
                    crate::obs::metrics::set_mcp_event_lag(&server, &name, now.saturating_sub(at));
                }
                if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                    let evicted = b.seen.insert(seen_key, now);
                    crate::obs::metrics::record_mcp_events_dedup_evicted(&server, &name, evicted);
                    if b.rate.is_some() {
                        b.tokens = (b.tokens - 1.0).max(0.0);
                    }
                    if !schema_ok && !b.schema_relisted {
                        b.schema_relist = true;
                        b.schema_relisted = true;
                    }
                }
                if !schema_ok {
                    self.log.warn(
                        "mcp.events.schema_mismatch",
                        json!({"server": server, "event_type": name, "event_id": tame(&event_id)}),
                    );
                }
                Taken::Appended
            }
            // Over the store's limit: no retry of it fits, so it is passed
            // over, said like an oversize one.
            Err(crate::runtime::streams::AppendError::TooLarge(_)) => {
                crate::obs::metrics::record_mcp_event(&server, &name, mode, "rejected");
                self.log.warn(
                    "mcp.events.rejected",
                    json!({"server": server, "event_type": name, "event_id": tame(&event_id), "cause": "oversize"}),
                );
                Taken::Rejected("oversize")
            }
            Err(crate::runtime::streams::AppendError::Refused(e)) => {
                crate::obs::metrics::record_mcp_event(&server, &name, mode, "refused_pressure");
                Taken::Refused(e)
            }
        }
    }

    /// A poll that failed: what the code says follows (§5.13).
    fn events_poll_failed(&mut self, key: &str, e: Failure) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        // List the server again; the binding is armed against what it then
        // lists, after the backoff this failure earns — a server that keeps
        // answering the same way is not asked in a loop.
        let rediscover = |rt: &mut Runtime| {
            if let Some(l) = rt.mcp_events.listings.get_mut(&server) {
                l.due_at = 0;
                l.descriptors = None;
            }
            if let Some(b) = rt.mcp_events.bindings.get_mut(key) {
                b.state = State::Discovering;
                b.failures += 1;
            }
        };
        match &e {
            Failure::Rpc(r) => match r.code {
                // The type is gone: list again, which refuses it if so.
                wire::code::NOT_FOUND => rediscover(self),
                wire::code::FORBIDDEN => {
                    self.events_refuse(key, "forbidden", &r.message);
                }
                wire::code::INVALID_PARAMS => {
                    self.events_refuse(key, "invalid_arguments", &r.message);
                }
                wire::code::METHOD_NOT_FOUND => {
                    self.events_refuse(key, "method_not_found", &r.message);
                }
                // The type changed in place: a cursor minted under the old
                // schema is answered so again, so it is let go — what happened
                // in between is lost, and said so [SEP L221].
                wire::code::UNSUPPORTED
                    if r.data
                        .as_ref()
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        == Some("schema_changed") =>
                {
                    if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                        b.cursor = None;
                        let _ = self.durable.delete(Kind::Memory, &b.cursor_key());
                    }
                    self.log.warn(
                        "mcp.events.gap",
                        json!({"server": server, "event_type": name, "cause": "schema_changed"}),
                    );
                    crate::obs::metrics::record_mcp_event_gap(&server, &name, "schema_changed");
                    rediscover(self);
                }
                wire::code::UNSUPPORTED => {
                    // A delivery mode the type does not offer; P2 has no
                    // other mode to move to.
                    self.events_refuse(key, "not_supported", &r.message);
                }
                code => {
                    let max = if code == wire::code::RESOURCE_EXHAUSTED {
                        EXHAUSTED_MAX_MS
                    } else {
                        BACKOFF_MAX_MS
                    };
                    let b = self.mcp_events.bindings.get_mut(key).expect("held above");
                    b.failures += 1;
                    let wait = backoff(b.failures, max);
                    b.due_at = now + wait;
                    let level_error = code == wire::code::HEADER_MISMATCH;
                    let line = json!({"server": server, "event_type": name, "code": code,
                                      "err": tame(&r.message), "retry_ms": wait});
                    if level_error {
                        self.log.error("mcp.events.poll.fail", line);
                    } else {
                        self.log.warn("mcp.events.poll.fail", line);
                    }
                }
            },
            Failure::Transport(m) => {
                b.failures += 1;
                let wait = backoff(b.failures, BACKOFF_MAX_MS);
                b.due_at = now + wait;
                self.log.warn(
                    "mcp.events.poll.fail",
                    json!({"server": server, "event_type": name, "err": tame(m), "retry_ms": wait}),
                );
            }
            Failure::TooLarge => {}
        }
    }
}

/// Runtime subscriptions (RFC 0045 §5.15): made by a `wait` that subscribes
/// or by the `events.*` tools, always inside an operator grant, sharing a
/// binding's identity, records and driver.
impl Runtime {
    /// Every grant the settings hold, by `(server, event type)`.
    fn events_grants(
        &self,
    ) -> BTreeMap<(String, String), crate::config::settings::events::EventGrant> {
        self.settings
            .mcp
            .servers
            .iter()
            .flat_map(|s| {
                s.event_grants
                    .iter()
                    .map(move |g| ((s.name.clone(), g.name.clone()), g.clone()))
            })
            .collect()
    }

    /// Subscribe at runtime: `arguments` merged under the grant's `fixed`
    /// ones (a runtime value for a fixed key is refused, not overridden),
    /// checked against the type's `inputSchema` where the server is listed,
    /// within the grant's `max` and the instance cap. Equal identities share
    /// one subscription: `owner` joins it.
    pub(crate) fn events_subscribe_runtime(
        &mut self,
        server: &str,
        event: &str,
        arguments: Map<String, Value>,
        owner: Owner,
    ) -> Result<Subscribed, String> {
        let grant = self
            .events_grants()
            .remove(&(server.to_string(), event.to_string()))
            .ok_or_else(|| {
                format!(
                    "no `event_grants` entry on mcp server {server:?} grants {event:?} — runtime subscriptions are made only inside an operator grant"
                )
            })?;
        if let Some(k) = arguments.keys().find(|k| grant.fixed.contains_key(*k)) {
            return Err(format!(
                "argument {k:?} is fixed by the operator's grant; it cannot be set at runtime"
            ));
        }
        let mut merged = grant.fixed.clone();
        merged.extend(arguments);
        let size = serde_json::to_string(&merged).map_or(usize::MAX, |t| t.len());
        if size > MAX_ARGUMENT_BYTES {
            return Err(format!(
                "the arguments are {size} bytes serialized; a runtime subscription's are at most {MAX_ARGUMENT_BYTES}"
            ));
        }
        if let Some(schema) = self
            .mcp_events
            .listings
            .get(server)
            .and_then(|l| l.descriptors.as_ref())
            .and_then(|d| d.get(event))
            .and_then(|d| d.input_schema.as_ref())
            && let Err(errs) = crate::jsonschema::validate(schema, &Value::Object(merged.clone()))
        {
            return Err(format!(
                "the arguments do not fit the type's inputSchema: {}",
                crate::jsonschema::explain(&errs)
            ));
        }
        let key = bridge::binding_hash(server, event, &merged);
        // Owners that are gone let go first: the slot a finished wait or an
        // expired tool hold kept is free for this one.
        self.mcp_events.swept_at = 0;
        self.events_sweep_owners(now_ms());
        let held = |b: &Binding| b.owners.iter().any(Owner::is_runtime);
        let existing = self.mcp_events.bindings.get(&key);
        if existing.is_none_or(|b| !held(b)) {
            let live = self
                .mcp_events
                .bindings
                .values()
                .filter(|b| b.server == server && b.name == event && held(b))
                .count();
            if live >= grant.max as usize {
                return Err(format!(
                    "the grant allows {} live subscriptions of {event:?} on {server:?}, and all are held",
                    grant.max
                ));
            }
        }
        if !self.mcp_events.bindings.contains_key(&key) {
            if self.mcp_events.bindings.len() >= crate::config::settings::events::MAX_SUBSCRIPTIONS
            {
                return Err(format!(
                    "this instance holds {} subscriptions, its cap",
                    crate::config::settings::events::MAX_SUBSCRIPTIONS
                ));
            }
            let fresh = Binding::new(
                key.clone(),
                server,
                event,
                merged,
                Shape::of_grant(&grant),
                now_ms(),
            );
            self.mcp_events.bindings.insert(key.clone(), fresh);
        }
        let b = self
            .mcp_events
            .bindings
            .get_mut(&key)
            .expect("inserted above");
        // One hold per holder: the same caller subscribing again renews it.
        let prior = b.owners.iter().find(|o| o.same_holder(&owner)).cloned();
        if let Some(p) = &prior {
            b.owners.remove(p);
        }
        b.owners.insert(owner.clone());
        // Joining one only the instruction follows: its occurrences land in
        // the grant's stream from now on.
        if b.into.is_none() {
            b.apply(Shape::of_grant(&grant));
        }
        let into = b.into.clone().unwrap_or_else(|| grant.into.clone());
        let out = Subscribed {
            key: key.clone(),
            stream: into.stream,
            subject: into.subject,
        };
        if prior.is_none() {
            self.events_said("mcp.events.subscribed", &key, &owner, "subscribed");
        }
        if prior.as_ref() != Some(&owner) {
            self.events_save_runtime();
        }
        if self.mcp_events.pool.is_none() {
            self.mcp_events.pool = Some(Pool::start(self.events_tx.clone()));
        }
        // Held against the server's listing now, when it has one.
        if self.mcp_events.bindings[&key].state == State::Discovering {
            let refused = self
                .mcp_events
                .listings
                .get(server)
                .and_then(|l| l.refused.clone());
            match refused {
                Some((reason, detail)) => self.events_refuse(&key, reason, &detail),
                None => self.events_evaluate(&key),
            }
        }
        // Refused at once: nothing will arrive, and the caller hears it now.
        if let Some(State::Refused(reason)) =
            self.mcp_events.bindings.get(&key).map(|b| b.state.clone())
        {
            self.events_release(&key, &owner, "refused");
            return Err(format!(
                "the subscription is refused ({reason}); mcp.events.refused says why"
            ));
        }
        Ok(out)
    }

    /// `owner` lets go of `key`; with the last owner, the subscription stops.
    /// A wait whose subscription is taken from it — the grant removed or
    /// narrowed, the subscription refused — fails: it would wait for nothing.
    pub(crate) fn events_release(&mut self, key: &str, owner: &Owner, reason: &str) {
        let held = self
            .mcp_events
            .bindings
            .get_mut(key)
            .is_some_and(|b| b.owners.remove(owner));
        if held {
            self.events_said("mcp.events.unsubscribed", key, owner, reason);
            if self.mcp_events.bindings[key].owners.is_empty() {
                self.events_remove(key, reason);
            } else if self.events_fall_back_to_instruction(key) {
                self.events_sync_routes();
                self.events_hold_pending();
            }
            self.events_save_runtime();
        }
        // After the release, so the wait's own leaving finds nothing to let go.
        if let Owner::Wait { run, step } = owner
            && reason != "wait_left"
        {
            self.events_fail_wait(run, step, key, reason);
        }
    }

    /// Stop a binding no owner holds: its records go.
    fn events_remove(&mut self, key: &str, reason: &str) {
        // A webhook subscription is let go eagerly; later deliveries to its
        // route are answered `410`.
        self.events_hook_stop(key, true);
        self.mcp_events
            .hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
        if let Some(b) = self.mcp_events.bindings.remove(key) {
            let _ = self.durable.delete(Kind::Memory, &b.cursor_key());
            let _ = self.durable.delete(Kind::Memory, &b.seen_key());
            self.log.info(
                "mcp.events.removed",
                json!({"server": b.server, "event_type": b.name, "reason": reason}),
            );
        }
    }

    /// A subscribe or an unsubscribe, logged and audited: it creates or
    /// removes a subscription on a third party under a credential.
    fn events_said(&mut self, event: &str, key: &str, owner: &Owner, reason: &str) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let line = json!({"server": b.server, "event_type": b.name, "owner": owner.label(),
                          "reason": reason, "subscription": key});
        self.audit(crate::runtime::audit::AuditEvent {
            action: event,
            target: line.clone(),
            outcome: "ok",
            principal: Some("system"),
            role: None,
            request_id: None,
            sid: None,
        });
        self.log.info(event, line);
    }

    /// Keep the runtime-owned bindings across a restart: who holds what.
    fn events_save_runtime(&mut self) {
        let held: Vec<Value> = self
            .mcp_events
            .bindings
            .values()
            .filter_map(|b| {
                let owners: Vec<&Owner> = b.owners.iter().filter(|o| o.is_runtime()).collect();
                (!owners.is_empty()).then(|| {
                    json!({"key": b.key, "server": b.server, "name": b.name,
                           "arguments": b.arguments, "owners": owners})
                })
            })
            .collect();
        let saved = if held.is_empty() {
            self.durable.delete(Kind::Memory, RUNTIME_INDEX)
        } else {
            self.durable
                .put(Kind::Memory, RUNTIME_INDEX, json!({"bindings": held}), None)
                .map(|_| ())
        };
        // Said, not swallowed: a stale index restores what was released and
        // loses what was made, and the waits on those fail at the next boot.
        if let Err(e) = saved {
            self.log.warn(
                "mcp.events.index.failed",
                json!({"subscriptions": held.len(), "err": e.to_string()}),
            );
        }
    }

    /// Bring back what the runtime held before a restart, for every grant
    /// that still allows it: a wait still suspended resumes from its
    /// subscription's cursor, a tool's subscription until it expires.
    fn events_restore_runtime(&mut self, now: u64) {
        let Ok(Some(index)) = self.durable.get(Kind::Memory, RUNTIME_INDEX) else {
            return;
        };
        let grants = self.events_grants();
        for entry in index.state["bindings"].as_array().into_iter().flatten() {
            let (Some(key), Some(server), Some(name)) = (
                entry["key"].as_str(),
                entry["server"].as_str(),
                entry["name"].as_str(),
            ) else {
                continue;
            };
            let owners: Vec<Owner> = entry["owners"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| serde_json::from_value(o.clone()).ok())
                .collect();
            let Some(grant) = grants.get(&(server.to_string(), name.to_string())) else {
                // The grant is gone: its waits fail (at the reconciliation
                // that follows), and its records go with it.
                let _ = self
                    .durable
                    .delete(Kind::Memory, &format!("_mcpev/{key}/cursor"));
                let _ = self
                    .durable
                    .delete(Kind::Memory, &format!("_mcpev/{key}/seen"));
                continue;
            };
            if owners.is_empty() {
                continue;
            }
            let arguments = entry["arguments"].as_object().cloned().unwrap_or_default();
            let mut b = Binding::new(
                key.to_string(),
                server,
                name,
                arguments,
                Shape::of_grant(grant),
                now,
            );
            b.owners.extend(owners);
            self.mcp_events.bindings.insert(key.to_string(), b);
        }
    }

    /// Every few seconds: let go for the owners that are gone — a wait no
    /// longer suspended on its subscription, a tool's subscription past its
    /// lifetime, or one made by a run that ended.
    fn events_sweep_owners(&mut self, now: u64) {
        if now < self.mcp_events.swept_at + SWEEP_EVERY_MS {
            return;
        }
        self.mcp_events.swept_at = now;
        let mut waiting = std::collections::BTreeSet::new();
        for (rid, run) in &self.runs {
            if run.status.is_terminal() {
                continue;
            }
            for (sid, st) in &run.steps {
                if st.status == crate::engine::run::StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "event"
                    && let Some(key) = w["subscription"].as_str()
                {
                    waiting.insert((rid.clone(), sid.clone(), key.to_string()));
                }
            }
        }
        let live_run = |run: &str| self.runs.get(run).is_some_and(|r| !r.status.is_terminal());
        let mut gone: Vec<(String, Owner, &'static str)> = Vec::new();
        for b in self.mcp_events.bindings.values() {
            for o in &b.owners {
                let why = match o {
                    Owner::Config | Owner::Instruction => None,
                    Owner::Wait { run, step } => {
                        (!waiting.contains(&(run.clone(), step.clone(), b.key.clone())))
                            .then_some("wait_left")
                    }
                    Owner::Tool {
                        run, expires_at, ..
                    } => {
                        if *expires_at <= now {
                            Some("expired")
                        } else if run.as_deref().is_some_and(|r| !live_run(r)) {
                            Some("run_ended")
                        } else {
                            None
                        }
                    }
                };
                if let Some(why) = why {
                    gone.push((b.key.clone(), o.clone(), why));
                }
            }
        }
        for (key, owner, why) in gone {
            self.events_release(&key, &owner, why);
        }
    }

    /// The servers Events talks to: every one a binding names, and every one
    /// with a grant — listed before anything subscribes, so a runtime
    /// subscription meets its type's `inputSchema` at once and `events.list`
    /// has types to offer.
    fn events_servers(&self) -> std::collections::BTreeSet<String> {
        let mut out: std::collections::BTreeSet<String> = self
            .mcp_events
            .bindings
            .values()
            .map(|b| b.server.clone())
            .collect();
        out.extend(
            self.settings
                .mcp
                .servers
                .iter()
                .filter(|s| !s.event_grants.is_empty())
                .map(|s| s.name.clone()),
        );
        out
    }

    /// Whether the wait `(run, step)` is suspended on subscription `key`.
    fn events_wait_holds(&self, run: &str, step: &str, key: &str) -> bool {
        self.runs
            .get(run)
            .filter(|r| !r.status.is_terminal())
            .and_then(|r| r.steps.get(step))
            .is_some_and(|st| {
                st.status == crate::engine::run::StepStatus::Suspended
                    && st
                        .wait
                        .as_ref()
                        .is_some_and(|w| w["subscription"].as_str() == Some(key))
            })
    }

    /// Fail a wait suspended on `key` whose subscription ended under it.
    /// Leaving the wait lets go of it (`finish_step`).
    fn events_fail_wait(&mut self, run: &str, step: &str, key: &str, why: &str) {
        if !self.events_wait_holds(run, step, key) {
            return;
        }
        let what = match why {
            "grant_removed" => "the operator's grant was removed",
            "grant_narrowed" => "the operator's grant no longer allows it",
            "grant_max" => "the operator's grant allows fewer live subscriptions now",
            "instance_cap" => "this instance holds its cap of subscriptions",
            "refused" => "the server refuses it (mcp.events.refused says why)",
            "not_restored" => "it was not kept across the restart",
            other => other,
        };
        self.finish_step_pub(
            run,
            step,
            crate::engine::run::StepStatus::Failed,
            None,
            Some(format!("wait subscribe: its subscription ended — {what}")),
            0,
        );
    }

    /// A subscription's stream changed under the waits on it (its grant's or
    /// binding's `into` was edited): they wait on the new one, from its head
    /// now.
    fn events_follow_stream(&mut self, key: &str) {
        let Some(EventInto { stream, subject }) = self
            .mcp_events
            .bindings
            .get(key)
            .and_then(|b| b.into.clone())
        else {
            return;
        };
        let b = &self.mcp_events.bindings[key];
        let waits: Vec<(String, String)> = b
            .owners
            .iter()
            .filter_map(|o| match o {
                Owner::Wait { run, step } => Some((run.clone(), step.clone())),
                _ => None,
            })
            .collect();
        let anchor = self
            .durable
            .manifest()
            .streams
            .get(&stream)
            .map(|m| m.seq)
            .unwrap_or(0);
        let mut moved = false;
        for (run, step) in waits {
            if !self.events_wait_holds(&run, &step, key) {
                continue;
            }
            if let Some(w) = self
                .runs
                .get_mut(&run)
                .and_then(|r| r.steps.get_mut(&step))
                .and_then(|st| st.wait.as_mut())
            {
                w["stream"] = json!(stream);
                w["subject"] = json!(subject);
                w["anchor"] = json!(anchor);
                moved = true;
            }
        }
        if moved {
            self.checkpoint(false);
        }
    }

    /// The caps, held after a reload or a restart changed what is live: each
    /// grant's `max` (it may have been lowered) and the instance's
    /// [`MAX_SUBSCRIPTIONS`] (static bindings added beside runtime ones). The
    /// runtime holds past them go; the config's never do.
    fn events_enforce_caps(&mut self) {
        use crate::config::settings::events::MAX_SUBSCRIPTIONS;
        let grants = self.events_grants();
        let held: Vec<(String, String, String, bool)> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.owners.iter().any(Owner::is_runtime))
            .map(|b| {
                (
                    b.key.clone(),
                    b.server.clone(),
                    b.name.clone(),
                    b.owners.iter().any(|o| !o.is_runtime()),
                )
            })
            .collect();
        let mut over: Vec<(String, &'static str)> = Vec::new();
        let mut counted: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (key, server, name, _) in &held {
            let n = counted.entry((server.clone(), name.clone())).or_default();
            *n += 1;
            if let Some(g) = grants.get(&(server.clone(), name.clone()))
                && *n > g.max as usize
            {
                over.push((key.clone(), "grant_max"));
            }
        }
        let mut total = self.mcp_events.bindings.len();
        for (key, _, _, shared) in &held {
            if total <= MAX_SUBSCRIPTIONS {
                break;
            }
            if !shared && !over.iter().any(|(k, _)| k == key) {
                over.push((key.clone(), "instance_cap"));
                total -= 1;
            }
        }
        for (key, why) in over {
            let owners: Vec<Owner> = self
                .mcp_events
                .bindings
                .get(&key)
                .map(|b| {
                    b.owners
                        .iter()
                        .filter(|o| o.is_runtime())
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for o in owners {
                self.events_release(&key, &o, why);
            }
        }
    }

    /// At boot, every wait suspended on a subscription holds it: one whose
    /// subscription is back is its owner again; one whose subscription was
    /// not kept (its grant gone, the index lost) fails rather than wait for
    /// nothing.
    fn events_reconcile_waits(&mut self) {
        let mut waits: Vec<(String, String, String)> = Vec::new();
        for (rid, run) in &self.runs {
            if run.status.is_terminal() {
                continue;
            }
            for (sid, st) in &run.steps {
                if st.status == crate::engine::run::StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "event"
                    && let Some(key) = w["subscription"].as_str()
                {
                    waits.push((rid.clone(), sid.clone(), key.to_string()));
                }
            }
        }
        let mut owned = false;
        for (run, step, key) in waits {
            let owner = Owner::Wait {
                run: run.clone(),
                step: step.clone(),
            };
            match self.mcp_events.bindings.get_mut(&key) {
                Some(b) => owned |= b.owners.insert(owner),
                None => self.events_fail_wait(&run, &step, &key, "not_restored"),
            }
        }
        if owned {
            self.events_save_runtime();
        }
    }
}

/// The `events.*` tools (RFC 0045 §5.15 b): the model subscribes, inside a
/// grant that offers them (`tools: true`), to event types whose occurrences
/// land in the grant's stream. A subscription is owned by the calling
/// context — the root conversation, or a workflow run — and lives
/// until it is unsubscribed, the grant's lifetime passes, or (for a step)
/// the run ends.
impl Runtime {
    pub(crate) fn events_tool(
        &mut self,
        caller: &super::tools::ToolCaller,
        name: &str,
        args: Value,
    ) -> super::tools::ToolOutcome {
        use super::tools::ToolOutcome;
        let ok = |v: Value| ToolOutcome::Ready(v, false);
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        if caller.subagent.is_some() {
            return err(format!("{name} is not offered to subagents"));
        }
        // A run's steps share what the run subscribed to: any of them may
        // list or stop it, and it ends with the run.
        let context = match &caller.run {
            Some(r) => format!("run:{r}"),
            None => format!("ctx:{}", caller.context_id()),
        };
        let offered: Vec<(String, crate::config::settings::events::EventGrant)> = self
            .settings
            .mcp
            .servers
            .iter()
            .flat_map(|s| {
                s.event_grants
                    .iter()
                    .filter(|g| g.tools)
                    .map(move |g| (s.name.clone(), g.clone()))
            })
            .collect();
        match name {
            "events.list" => {
                let only = args.get("server").and_then(Value::as_str);
                let types: Vec<Value> = offered
                    .iter()
                    .filter(|(s, _)| only.is_none_or(|o| o == s))
                    .map(|(server, g)| {
                        let d = self
                            .mcp_events
                            .listings
                            .get(server)
                            .and_then(|l| l.descriptors.as_ref())
                            .and_then(|d| d.get(&g.name));
                        // What the grant fixes is not the model's to set, so
                        // it is not offered as an input either.
                        let input = d.and_then(|d| d.input_schema.clone()).map(|mut s| {
                            for k in g.fixed.keys() {
                                if let Some(p) =
                                    s.get_mut("properties").and_then(Value::as_object_mut)
                                {
                                    p.remove(k);
                                }
                                if let Some(r) = s.get_mut("required").and_then(Value::as_array_mut)
                                {
                                    r.retain(|x| x.as_str() != Some(k));
                                }
                            }
                            s
                        });
                        json!({
                            "server": server,
                            "event": g.name,
                            "description": d.and_then(|d| d.description.clone()),
                            "inputSchema": input,
                            "payloadSchema": d.and_then(|d| d.payload_schema.clone()),
                            "listed": d.is_some(),
                            "stream": g.into.stream,
                            "subject": g.into.subject,
                        })
                    })
                    .collect();
                ok(json!({"types": types}))
            }
            "events.subscribe" => {
                let (Some(server), Some(event)) = (
                    args.get("server").and_then(Value::as_str),
                    args.get("event").and_then(Value::as_str),
                ) else {
                    return err("events.subscribe needs `server` and `event`".into());
                };
                let Some((_, grant)) = offered.iter().find(|(s, g)| s == server && g.name == event)
                else {
                    return err(format!(
                        "no grant offers {event:?} on {server:?} to the events tools (event_grants with tools: true)"
                    ));
                };
                // A step's `servers:` is a cap: subscribing sends arguments
                // to the server, so it is reaching it.
                if let (Some(r), Some(s)) = (&caller.run, &caller.step)
                    && let Some(cap) = self.step_servers_cap(r, s)
                    && !cap.iter().any(|x| x == server)
                {
                    return err(format!("this step may not reach mcp server {server:?}"));
                }
                let arguments = args
                    .get("arguments")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let lifetime = grant
                    .lifetime
                    .as_ref()
                    .map_or(DEFAULT_LIFETIME_MS, |d| d.0.as_millis() as u64);
                let expires_at = now_ms().saturating_add(lifetime);
                let owner = Owner::Tool {
                    context,
                    run: caller.run.clone(),
                    expires_at,
                };
                match self.events_subscribe_runtime(server, event, arguments, owner) {
                    Ok(s) => ok(json!({"subscription": s.key, "stream": s.stream,
                                       "subject": s.subject, "expires_at": expires_at})),
                    Err(e) => err(e),
                }
            }
            "events.unsubscribe" => {
                let Some(key) = args.get("subscription").and_then(Value::as_str) else {
                    return err("events.unsubscribe needs `subscription`".into());
                };
                let mine = self.mcp_events.bindings.get(key).and_then(|b| {
                    b.owners
                        .iter()
                        .find(|o| matches!(o, Owner::Tool { context: c, .. } if *c == context))
                        .cloned()
                });
                match mine {
                    Some(owner) => {
                        self.events_release(key, &owner, "unsubscribed");
                        ok(json!({"stopped": true}))
                    }
                    None => err(format!("{key:?} is not a subscription this caller holds")),
                }
            }
            "events.subscriptions" => {
                let held: Vec<Value> = self
                    .mcp_events
                    .bindings
                    .values()
                    .filter_map(|b| {
                        b.owners.iter().find_map(|o| match o {
                            Owner::Tool {
                                context: c,
                                expires_at,
                                ..
                            } if *c == context => Some(json!({
                                "subscription": b.key, "server": b.server, "event": b.name,
                                "arguments": b.arguments,
                                "stream": b.into.as_ref().map(|i| &i.stream),
                                "subject": b.into.as_ref().map(|i| &i.subject),
                                "expires_at": expires_at,
                                "state": b.state.name(),
                            })),
                            _ => None,
                        })
                    })
                    .collect();
                ok(json!({"subscriptions": held}))
            }
            other => err(format!(
                "internal tool {other:?} has no built-in implementation"
            )),
        }
    }
}

/// How long a tool-made subscription lives when its grant names no
/// `lifetime`.
const DEFAULT_LIFETIME_MS: u64 = 24 * 3600 * 1000;

impl Runtime {
    /// The `servers:` a run's step lists — its cap — when it lists any.
    fn step_servers_cap(&self, run: &str, step: &str) -> Option<Vec<String>> {
        let w = self.definition_for_run(run)?;
        let s = w.steps.get(step)?;
        s.spec.get("servers").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    }
}
