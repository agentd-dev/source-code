// SPDX-License-Identifier: AGPL-3.0-only
//! OTLP span export with the GenAI semantic conventions. [feature: otel]
//!
//! Hand-rolled OTLP-over-HTTP/**JSON** — no `opentelemetry` crate, no protobuf.
//! It reuses what agentd already has: the W3C trace/span ids of every unit of
//! work ([`crate::obs::trace`]), `serde_json`, and the hand-rolled HTTP client
//! ([`crate::net::http`]), over the same TLS dial as every other outbound
//! client for an `https://` collector (`net::tls`, with `security.tls_ca`
//! added to the bundled roots). So `--features otel` stays dependency-free.
//!
//! **The spans are one tree.** Each span's id is the id that span already has
//! everywhere else: on the wire, in the `traceparent` agentd sends, and as
//! `span_id` on the log lines about it. The supervisor exports a span per
//! workflow run, per step attempt, per conversation turn and per request it
//! serves; a turn worker or a subagent exports its `invoke_agent` span — the
//! span its spawn payload names as its parent is the step's or the turn's —
//! with a `chat` span per model call and an `execute_tool` span per tool call;
//! every request agentd sends for a unit of work — a model call, an MCP call,
//! an A2A request, an `http` step, a push, a stream forward — is a CLIENT
//! span whose id is the span id it sent, and what an MCP connection asks for
//! itself names the process's span. A line's `link` and `links` become the
//! links of the span the line is in, or of a span of the line's own under it
//! when that span has already ended. A trace that is not sampled (flags
//! `00`) exports nothing.
//!
//! Off the default path: [`Span`] and [`RunSpan`] are **no-op handles unless
//! built `--features otel`** and an endpoint is set (so call sites stay clean
//! and the default build pays nothing). A span is queued when it ends — a run
//! when the run ends, a step when its attempt ends — and exported from the
//! queue: by a background thread every few seconds in the supervisor
//! ([`arm_traces`]), and at the end of its turn in a turn worker or a
//! subagent ([`RunSpan::finish`]), so no export ever runs on the reactor.
//! Best-effort: telemetry never fails the work it describes, a batch that
//! fails is dropped, not retried, and an exit waits at most [`LAST_EXPORT`]
//! on its last exports. The failure is logged: `otel.export.failed`
//! names the signal, the endpoint and the error when exports start failing,
//! then at most once a minute while the collector keeps failing, and
//! `otel.export.recovered` says when it takes a batch again, also at most
//! once a minute, so a collector that drops every other batch gets one line
//! of each a minute, not one every batch. Each process keeps its own count,
//! so a turn worker, which exports once, says it once.

use crate::obs::log::Logger;
use crate::obs::trace::TraceContext;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The configured OTLP endpoint (`observability.otel.endpoint`), set once by
/// the process that exports spans: the supervisor from its settings, a turn
/// worker or a subagent from its spawn payload.
static ENDPOINT: OnceLock<String> = OnceLock::new();

/// Record the configured OTLP endpoint for this process's span export. The
/// environment's `OTEL_EXPORTER_OTLP_ENDPOINT` is the fallback when none is
/// configured. Set before the first span begins.
pub fn set_endpoint(endpoint: Option<String>) {
    if let Some(ep) = endpoint.filter(|e| !e.is_empty()) {
        let _ = ENDPOINT.set(ep);
    }
}

/// Where this process exports spans, if anywhere: the configured endpoint,
/// else the environment's, read once — the endpoint is restart-only.
#[cfg(feature = "otel")]
fn endpoint() -> Option<&'static str> {
    static FROM_ENV: OnceLock<Option<String>> = OnceLock::new();
    ENDPOINT.get().map(String::as_str).or_else(|| {
        FROM_ENV
            .get_or_init(|| {
                std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .as_deref()
    })
}

/// Whether this process exports spans at all. Always `false` without `otel`.
pub fn exporting() -> bool {
    #[cfg(feature = "otel")]
    {
        endpoint().is_some()
    }
    #[cfg(not(feature = "otel"))]
    {
        false
    }
}

/// Current time as unix nanoseconds — a span start/end stamp.
pub fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Epoch milliseconds — how runs and steps keep their times — as a span stamp.
pub fn ms_to_nanos(ms: u64) -> u128 {
    u128::from(ms) * 1_000_000
}

/// What a span is to the work around it: the OTLP `SpanKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Work inside agentd: a run, a step, a turn, the process.
    Internal,
    /// Work agentd does for a request it serves: an A2A request, a webhook
    /// delivery, and the run or turn such a request starts.
    Server,
    /// A request agentd sends: a model call, an MCP call, an A2A request, an
    /// `http` step, a push, a stream forward.
    Client,
    /// An event agentd puts on a stream.
    Producer,
    /// A run a stream event starts.
    Consumer,
}

impl Kind {
    /// The OTLP `SpanKind` number.
    #[cfg(feature = "otel")]
    fn otlp(self) -> u8 {
        match self {
            Kind::Internal => 1,
            Kind::Server => 2,
            Kind::Client => 3,
            Kind::Producer => 4,
            Kind::Consumer => 5,
        }
    }
}

/// An attribute's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(s: &str) -> Attr {
        Attr::Str(s.to_string())
    }
}
impl From<String> for Attr {
    fn from(s: String) -> Attr {
        Attr::Str(s)
    }
}
impl From<&String> for Attr {
    fn from(s: &String) -> Attr {
        Attr::Str(s.clone())
    }
}
impl From<u64> for Attr {
    fn from(n: u64) -> Attr {
        Attr::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }
}
impl From<u32> for Attr {
    fn from(n: u32) -> Attr {
        Attr::Int(i64::from(n))
    }
}
impl From<u16> for Attr {
    fn from(n: u16) -> Attr {
        Attr::Int(i64::from(n))
    }
}
impl From<usize> for Attr {
    fn from(n: usize) -> Attr {
        Attr::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }
}
impl From<i64> for Attr {
    fn from(n: i64) -> Attr {
        Attr::Int(n)
    }
}
impl From<bool> for Attr {
    fn from(b: bool) -> Attr {
        Attr::Bool(b)
    }
}

/// One span to export: begun with the identity `trace` gives it — its trace
/// id, its own span id, its parent's — given attributes and links, and
/// ended, which queues it for this process's exporter. A no-op handle without
/// `otel`, without an endpoint, without a trace, or in a trace that is not
/// sampled: then nothing is built or queued.
#[must_use = "a span is exported only when it ends"]
pub struct Span {
    #[cfg(feature = "otel")]
    inner: Option<Box<imp::Span>>,
}

/// Begin the span `trace` is, named `name`, of `kind`, started at
/// `start_unix_nanos`.
pub fn span(trace: Option<&TraceContext>, name: &str, kind: Kind, start_unix_nanos: u128) -> Span {
    #[cfg(feature = "otel")]
    {
        let inner = trace
            .filter(|t| t.sampled() && exporting())
            .map(|t| Box::new(imp::Span::begin(t, name, kind, start_unix_nanos)));
        Span { inner }
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = (trace, name, kind, start_unix_nanos);
        Span {}
    }
}

impl Span {
    /// Whether this span will be exported: attributes that cost something to
    /// build are worth building only then.
    pub fn recording(&self) -> bool {
        #[cfg(feature = "otel")]
        {
            self.inner.is_some()
        }
        #[cfg(not(feature = "otel"))]
        {
            false
        }
    }

    /// Set the attribute `key`.
    pub fn attr(self, key: &'static str, value: impl Into<Attr>) -> Span {
        #[cfg(feature = "otel")]
        {
            let mut s = self;
            if let Some(i) = s.inner.as_mut() {
                i.attrs.push((key, value.into()));
            }
            s
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = (key, value);
            self
        }
    }

    /// Set the attribute `key` when there is a value.
    pub fn attr_opt<V: Into<Attr>>(self, key: &'static str, value: Option<V>) -> Span {
        match value {
            Some(v) => self.attr(key, v),
            None => self,
        }
    }

    /// Name where a request went — `url.scheme`, `server.address` and
    /// `server.port` — and not its URL: the URL of a push or a forward is
    /// often a credential (an incoming-webhook path, a `?token=`), and a span
    /// is kept and read more widely than the config it came from.
    pub fn destination(self, url: &str) -> Span {
        if !self.recording() {
            return self;
        }
        match crate::net::http::Url::parse(url) {
            Ok(u) => {
                // Credentials in the authority are not the address either.
                let host = u.host.rsplit('@').next().unwrap_or_default().to_string();
                self.attr("url.scheme", u.scheme)
                    .attr("server.address", host)
                    .attr("server.port", u.port)
            }
            Err(_) => self,
        }
    }

    /// Link the span a `traceparent` names. A malformed one is left out.
    pub fn link(self, traceparent: &str) -> Span {
        #[cfg(feature = "otel")]
        {
            let mut s = self;
            if let Some(i) = s.inner.as_mut() {
                i.add_link(traceparent);
            }
            s
        }
        #[cfg(not(feature = "otel"))]
        {
            let _ = traceparent;
            self
        }
    }

    /// End the span now, `ok` or failed, and queue it.
    pub fn end(self, ok: bool) {
        self.end_at(now_unix_nanos(), ok);
    }

    /// End the span at `end_unix_nanos` and queue it.
    pub fn end_at(self, end_unix_nanos: u128, ok: bool) {
        #[cfg(feature = "otel")]
        if let Some(i) = self.inner {
            imp::queue(*i, end_unix_nanos, ok);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (end_unix_nanos, ok);
    }
}

/// Note the links a log line `event` names in its `fields` — `link`, one
/// `traceparent`, and `links`, a list of them — on the span the line is in
/// (`ctx`'s), so they are exported with that span when it ends. A line about
/// a span that has already ended names them too late for it: they go out on
/// a span of their own, named for the line, under it. A link to the span's
/// own parent is left out: the parent is that edge already. A no-op unless
/// this process exports spans and the line's trace is sampled.
pub fn capture_links(event: &str, ctx: &crate::obs::log::LogCtx, fields: &serde_json::Value) {
    #[cfg(feature = "otel")]
    if exporting()
        && (fields.get("link").is_some_and(|l| !l.is_null()) || fields.get("links").is_some())
        && let Some(t) = ctx.trace().filter(TraceContext::sampled)
    {
        imp::capture_links(event, &t, fields);
    }
    #[cfg(not(feature = "otel"))]
    let _ = (event, ctx, fields);
}

/// Export every queued span now, on this thread, logging a failed export to
/// `log`: what a turn worker or a subagent does as its turn ends, and every
/// process as it exits. It waits at most [`LAST_EXPORT`] on the collector.
pub fn flush(log: &Logger) {
    #[cfg(feature = "otel")]
    imp::flush(log, Some(std::time::Instant::now() + LAST_EXPORT));
    #[cfg(not(feature = "otel"))]
    let _ = log;
}

/// How long an exit — a process's, or a turn's in a turn worker or a
/// subagent — waits on its last exports, spans and log records together.
/// What is not sent by then is dropped and counted lost, as is everything
/// behind a batch that fails: a collector that is down costs an exit this
/// long at most, not a timeout per batch.
pub const LAST_EXPORT: std::time::Duration = std::time::Duration::from_secs(3);

/// Arm the background export of this process's queued spans — the
/// supervisor's, so no export runs on the reactor. Idempotent; a no-op
/// without an endpoint or without `otel`.
pub fn arm_traces(log: &Logger) {
    #[cfg(feature = "otel")]
    imp::arm_traces(log);
    #[cfg(not(feature = "otel"))]
    let _ = log;
}

/// The process is exiting: export what is still queued, spans and log
/// records, on this thread, within [`LAST_EXPORT`].
pub fn shutdown(log: &Logger) {
    #[cfg(feature = "otel")]
    {
        let by = std::time::Instant::now() + LAST_EXPORT;
        imp::flush(log, Some(by));
        imp::flush_logs(log, Some(by));
    }
    #[cfg(not(feature = "otel"))]
    let _ = log;
}

/// One turn's `invoke_agent` span in a turn worker or a subagent. Begin it
/// with the turn's span, record a [`chat_span`] or a [`tool_span`] under it as
/// each model call and tool call ends — each with the span id it sent — then
/// `finish` it, which exports the turn. Every method is a **no-op
/// without the `otel` feature** or an endpoint, so the loop wires it
/// unconditionally with no `cfg` at the call sites.
pub struct RunSpan {
    #[cfg(feature = "otel")]
    inner: Option<(TraceContext, u128, Logger)>,
}

/// Begin the `invoke_agent` span `turn` (see [`crate::obs::trace::turn`]),
/// started at `start_unix_nanos`; a failed export is logged to `log`.
pub fn run_begin(log: &Logger, turn: Option<&TraceContext>, start_unix_nanos: u128) -> RunSpan {
    #[cfg(feature = "otel")]
    {
        RunSpan {
            inner: turn
                .filter(|t| t.sampled() && exporting())
                .map(|t| (t.clone(), start_unix_nanos, log.clone())),
        }
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = (log, turn, start_unix_nanos);
        RunSpan {}
    }
}

impl RunSpan {
    /// Close the `invoke_agent` span and export the turn: it and every span
    /// recorded under it, one OTLP batch. No-op without the feature/endpoint.
    pub fn finish(self, model: &str, input_tokens: u64, output_tokens: u64, ok: bool) {
        #[cfg(feature = "otel")]
        if let Some((turn, start, log)) = self.inner {
            span(Some(&turn), "invoke_agent", Kind::Internal, start)
                .attr("gen_ai.operation.name", "invoke_agent")
                .attr("gen_ai.request.model", model)
                .attr("gen_ai.usage.input_tokens", input_tokens)
                .attr("gen_ai.usage.output_tokens", output_tokens)
                .end(ok);
            imp::flush(&log, Some(std::time::Instant::now() + LAST_EXPORT));
        }
        #[cfg(not(feature = "otel"))]
        let _ = (model, input_tokens, output_tokens, ok);
    }
}

/// Record a `chat` span for one model call: `call` is the span its request
/// sent (a child of the turn's), so the provider's span is its child.
pub fn chat_span(
    call: Option<&TraceContext>,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    ok: bool,
    start_unix_nanos: u128,
) {
    span(call, "chat", Kind::Client, start_unix_nanos)
        .attr("gen_ai.operation.name", "chat")
        .attr("gen_ai.request.model", model)
        .attr("gen_ai.usage.input_tokens", input_tokens)
        .attr("gen_ai.usage.output_tokens", output_tokens)
        .end(ok);
}

/// Record an `execute_tool` span for one tool call: `call` is its span.
/// `mcp` is the MCP request the call made — its method, and the server when
/// one is known — which makes it a CLIENT span: the request sent it as its
/// `_meta.traceparent`.
pub fn tool_span(
    call: Option<&TraceContext>,
    tool_name: &str,
    mcp: Option<(&str, Option<&str>)>,
    ok: bool,
    start_unix_nanos: u128,
) {
    let kind = if mcp.is_some() {
        Kind::Client
    } else {
        Kind::Internal
    };
    span(call, "execute_tool", kind, start_unix_nanos)
        .attr("gen_ai.operation.name", "execute_tool")
        .attr("gen_ai.tool.name", tool_name)
        .attr_opt("mcp.method.name", mcp.map(|(method, _)| method))
        .attr_opt("agentd.mcp.server", mcp.and_then(|(_, server)| server))
        .end(ok);
}

/// Record the CLIENT span of one MCP request the supervisor made — a
/// `tools/call`, a `resources/read`, a `prompts/get` — for a step or a tool
/// call: `call` is the span its `_meta.traceparent` named, `target` the tool,
/// the resource or the prompt.
pub fn mcp_span(
    call: Option<&TraceContext>,
    method: &str,
    target: &str,
    server: &str,
    ok: bool,
    start_unix_nanos: u128,
) {
    let s = span(
        call,
        &format!("{method} {target}"),
        Kind::Client,
        start_unix_nanos,
    )
    .attr("mcp.method.name", method)
    .attr("agentd.mcp.server", server);
    let s = match method {
        "tools/call" => s
            .attr("gen_ai.operation.name", "execute_tool")
            .attr("gen_ai.tool.name", target),
        "resources/read" => s.attr("mcp.resource.uri", target),
        "prompts/get" => s.attr("gen_ai.prompt.name", target),
        _ => s,
    };
    s.end(ok);
}

/// Buffer one log record for OTLP export. A no-op unless
/// [`arm_logs`] installed the exporter (and always a no-op without `otel`).
pub fn capture_log(unix_nanos: u128, level: &str, event: &str, fields: &serde_json::Value) {
    #[cfg(feature = "otel")]
    imp::capture_log(unix_nanos, level, event, fields);
    #[cfg(not(feature = "otel"))]
    let _ = (unix_nanos, level, event, fields);
}

/// Arm the OTLP **logs** exporter: a bounded buffer drained by a background
/// thread to `<endpoint>/v1/logs`, which logs a failed export to `log`.
/// Idempotent; a no-op without `otel`.
pub fn arm_logs(log: &Logger, endpoint: &str, service: &str, version: &str) {
    #[cfg(feature = "otel")]
    imp::arm_logs(log, endpoint, service, version);
    #[cfg(not(feature = "otel"))]
    let _ = (log, endpoint, service, version);
}

#[cfg(feature = "otel")]
mod imp {
    use super::{Attr, Kind};
    use crate::net::http::{self, Url};
    use crate::obs::log::Logger;
    use crate::obs::quiet::{Health, Say};
    use crate::obs::trace::TraceContext;
    use serde_json::{Value, json};
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// A span, ready to encode as OTLP once it ends. Times are unix
    /// nanoseconds.
    pub(super) struct Span {
        pub trace_id: String,
        pub span_id: String,
        pub parent_span_id: Option<String>,
        pub tracestate: Option<String>,
        pub name: String,
        pub kind: Kind,
        pub start_unix_nanos: u128,
        pub end_unix_nanos: u128,
        pub ok: bool,
        pub attrs: Vec<(&'static str, Attr)>,
        /// `(trace id, span id)` of each linked span.
        pub links: Vec<(String, String)>,
    }

    /// The most links one span keeps (the OTel SDKs' default limit); the
    /// rest are dropped.
    const MAX_LINKS: usize = 128;

    impl Span {
        pub(super) fn begin(t: &TraceContext, name: &str, kind: Kind, start: u128) -> Span {
            Span {
                trace_id: t.trace_id.clone(),
                span_id: t.span_id.clone(),
                parent_span_id: t.parent_span_id.clone(),
                tracestate: t.tracestate.clone(),
                name: name.to_string(),
                kind,
                start_unix_nanos: start,
                end_unix_nanos: start,
                ok: true,
                attrs: Vec::new(),
                links: Vec::new(),
            }
        }

        pub(super) fn add_link(&mut self, traceparent: &str) {
            if self.links.len() < MAX_LINKS
                && let Some(t) = TraceContext::parse(traceparent, None)
                && !self
                    .links
                    .iter()
                    .any(|(tid, sid)| *tid == t.trace_id && *sid == t.span_id)
            {
                self.links.push((t.trace_id, t.span_id));
            }
        }
    }

    // ---- the span queue -----------------------------------------------------
    // Every span this process ends waits here for an export: the supervisor's
    // background thread takes it every few seconds; a turn worker or a
    // subagent takes it as its turn ends. Bounded, so a collector that is down
    // never grows memory: past the bound a new span is dropped.

    const QUEUE_CAP: usize = 8192;
    /// The most spans in one OTLP batch.
    const BATCH: usize = 512;

    static QUEUE: Mutex<Vec<Span>> = Mutex::new(Vec::new());

    /// End `s` and queue it, with the links its lines named.
    pub(super) fn queue(mut s: Span, end: u128, ok: bool) {
        s.end_unix_nanos = end.max(s.start_unix_nanos);
        s.ok = ok;
        for tp in end_links(&s.span_id) {
            s.add_link(&tp);
        }
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() < QUEUE_CAP {
            q.push(s);
        }
    }

    /// Export everything queued, in batches, on this thread — by `deadline`,
    /// when an exit waits on it (see [`send`]).
    pub(super) fn flush(log: &Logger, deadline: Option<Instant>) {
        let Some(endpoint) = super::endpoint() else {
            return;
        };
        let spans = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
        let mut batches = spans.chunks(BATCH);
        while let Some(batch) = batches.next() {
            let body = to_otlp_json(batch, "agentd", crate::VERSION);
            if !send(log, endpoint, "traces", &body, deadline, batches.len()) {
                break;
            }
        }
    }

    /// Spawn the thread that exports the queue every [`TRACES_EVERY`].
    pub(super) fn arm_traces(log: &Logger) {
        static ARMED: OnceLock<()> = OnceLock::new();
        if super::endpoint().is_none() || ARMED.set(()).is_err() {
            return;
        }
        let log = log.clone();
        std::thread::Builder::new()
            .name("otel-traces".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(TRACES_EVERY);
                    flush(&log, None);
                }
            })
            .ok();
    }

    /// How often the supervisor's queue is exported.
    const TRACES_EVERY: Duration = Duration::from_secs(2);

    // ---- links a line names ---------------------------------------------------
    // A line about a span may name another span as a cause it did not start
    // from — `link` — or several — `links`. They wait here, by the id of the
    // span the line is in, until that span ends and is queued with them. A
    // line about a span that has already ended — a gate answered after the
    // message that opened it was routed, a child reporting after its step —
    // is too late for it, so the spans this process has ended are kept here
    // too. Bounded: a span that never ends here (one a restart lost) gives
    // its place up to newer ones, and so does the oldest ended span.

    const LINKED_SPANS_CAP: usize = 4096;
    const ENDED_CAP: usize = 4096;

    #[derive(Default)]
    struct Links {
        by_span: HashMap<String, Vec<String>>,
        order: VecDeque<String>,
        ended: HashSet<String>,
        ended_order: VecDeque<String>,
    }

    static LINKS: Mutex<Option<Links>> = Mutex::new(None);

    pub(super) fn capture_links(event: &str, line: &TraceContext, fields: &Value) {
        let mut named: Vec<&str> = Vec::new();
        if let Some(l) = fields.get("link").and_then(Value::as_str) {
            named.push(l);
        }
        if let Some(ls) = fields.get("links").and_then(Value::as_array) {
            named.extend(ls.iter().filter_map(Value::as_str));
        }
        named.retain(|l| {
            !TraceContext::parse(l, None).is_some_and(|k| {
                k.trace_id == line.trace_id
                    && line.parent_span_id.as_deref() == Some(k.span_id.as_str())
            })
        });
        if named.is_empty() {
            return;
        }
        {
            let mut g = LINKS.lock().unwrap_or_else(|e| e.into_inner());
            let p = g.get_or_insert_with(Links::default);
            if !p.ended.contains(&line.span_id) {
                if !p.by_span.contains_key(&line.span_id) {
                    if p.order.len() >= LINKED_SPANS_CAP
                        && let Some(old) = p.order.pop_front()
                    {
                        p.by_span.remove(&old);
                    }
                    p.order.push_back(line.span_id.clone());
                }
                let links = p.by_span.entry(line.span_id.clone()).or_default();
                for l in named {
                    if links.len() < MAX_LINKS && !links.iter().any(|k| k == l) {
                        links.push(l.to_string());
                    }
                }
                return;
            }
        }
        // The line's span has gone out without them: they go out on a span
        // of the line's own, under it.
        let now = super::now_unix_nanos();
        let mut s = Span::begin(&line.child(), event, Kind::Internal, now);
        for l in named {
            s.add_link(l);
        }
        queue(s, now, true);
    }

    /// The links the lines about `span` named, now that it ends — which is
    /// recorded, so a line about it from now on goes out on its own span.
    fn end_links(span: &str) -> Vec<String> {
        let mut g = LINKS.lock().unwrap_or_else(|e| e.into_inner());
        let p = g.get_or_insert_with(Links::default);
        if p.ended.insert(span.to_string()) {
            p.ended_order.push_back(span.to_string());
            if p.ended_order.len() > ENDED_CAP
                && let Some(old) = p.ended_order.pop_front()
            {
                p.ended.remove(&old);
            }
        }
        match p.by_span.remove(span) {
            Some(links) => {
                p.order.retain(|s| s != span);
                links
            }
            None => Vec::new(),
        }
    }

    // ---- encoding -----------------------------------------------------------

    /// OTLP `AnyValue` for an attribute.
    fn any_value(a: &Attr) -> Value {
        match a {
            Attr::Str(s) => json!({ "stringValue": s }),
            // OTLP ints are stringified on the wire.
            Attr::Int(n) => json!({ "intValue": n.to_string() }),
            Attr::Bool(b) => json!({ "boolValue": b }),
        }
    }

    /// Encode spans as an OTLP `ExportTraceServiceRequest` body (`resourceSpans`).
    pub(super) fn to_otlp_json(spans: &[Span], service: &str, version: &str) -> Value {
        let encoded: Vec<Value> = spans.iter().map(encode_span).collect();
        json!({
            "resourceSpans": [{
                "resource": { "attributes": [
                    { "key": "service.name", "value": { "stringValue": service } },
                    { "key": "service.version", "value": { "stringValue": version } },
                ]},
                "scopeSpans": [{ "scope": { "name": "agentd" }, "spans": encoded }]
            }]
        })
    }

    fn encode_span(s: &Span) -> Value {
        let attrs: Vec<Value> = s
            .attrs
            .iter()
            .map(|(k, v)| json!({ "key": k, "value": any_value(v) }))
            .collect();
        let mut span = json!({
            "traceId": s.trace_id,
            "spanId": s.span_id,
            "name": s.name,
            "kind": s.kind.otlp(),
            "startTimeUnixNano": s.start_unix_nanos.to_string(),
            "endTimeUnixNano": s.end_unix_nanos.to_string(),
            "status": { "code": if s.ok { 1 } else { 2 } }, // OK / ERROR
            "attributes": attrs,
        });
        if let Some(p) = &s.parent_span_id {
            span["parentSpanId"] = json!(p);
        }
        if let Some(ts) = &s.tracestate {
            span["traceState"] = json!(ts);
        }
        if !s.links.is_empty() {
            span["links"] = s
                .links
                .iter()
                .map(|(t, sp)| json!({ "traceId": t, "spanId": sp }))
                .collect();
        }
        span
    }

    /// The bound on each step of one export: the dial, each write, each read.
    const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Less time than this left before an exit's deadline is none.
    const LEAST_SEND: Duration = Duration::from_millis(50);

    /// POST an OTLP body to `<endpoint>/v1/<signal>` (`traces` | `logs`),
    /// over TLS for an `https://` endpoint, each step of it bounded by
    /// `timeout`.
    fn export(endpoint: &str, signal: &str, body: &Value, timeout: Duration) -> Result<(), String> {
        let base = endpoint.trim_end_matches('/');
        let suffix = format!("/v1/{signal}");
        let target = if base.ends_with(&suffix) {
            base.to_string()
        } else {
            format!("{base}{suffix}")
        };
        let url = Url::parse(&target).map_err(|e| format!("bad endpoint {target}: {e}"))?;
        let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut stream = connect(&url, timeout)?;
        let headers = [("content-type", "application/json")];
        let resp = http::send(
            stream.as_mut(),
            &url.host_header(),
            "POST",
            &url.path,
            &headers,
            &bytes,
        )
        .map_err(|e| format!("POST {target}: {e}"))?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(format!("POST {target}: HTTP {}", resp.status))
        }
    }

    /// Dial the collector: plain TCP for `http://`, TLS for `https://` with
    /// the trust every outbound client has (the bundled roots, plus
    /// `security.tls_ca`). The handshake runs with the request's first write.
    fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
        #[cfg(not(feature = "tls"))]
        if url.is_tls() {
            return Err("an https:// collector needs the tls build feature".into());
        }
        let tcp = http::connect_tcp(&url.host, url.port, timeout)
            .map_err(|e| format!("connect {}: {e}", url.host_header()))?;
        #[cfg(feature = "tls")]
        if url.is_tls() {
            let tls = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| format!("tls {}: {e}", url.host))?;
            return Ok(Box::new(tls));
        }
        Ok(Box::new(tcp))
    }

    // ---- export failures ---------------------------------------------------
    // A batch that fails is dropped; the log says so without repeating itself
    // every batch while a collector stays down or keeps failing some batches
    // (`obs::quiet`: one line of each a minute, per signal).

    static TRACES_HEALTH: Mutex<Health> = Mutex::new(Health::new());
    static LOGS_HEALTH: Mutex<Health> = Mutex::new(Health::new());

    /// Export one batch of `signal`, log what its outcome calls for, and say
    /// whether to send the `rest` behind it. With a `deadline` — an exit
    /// waiting on the export — each step of the send is bounded by what is
    /// left of it, and a batch that fails, or finds no time left, is lost
    /// with the `rest`: the exit does not wait out a collector that is down
    /// one batch after another.
    fn send(
        log: &Logger,
        endpoint: &str,
        signal: &str,
        body: &Value,
        deadline: Option<Instant>,
        rest: usize,
    ) -> bool {
        let timeout = match deadline {
            None => Some(EXPORT_TIMEOUT),
            Some(by) => {
                let left = by.saturating_duration_since(Instant::now());
                (left >= LEAST_SEND).then(|| left.min(EXPORT_TIMEOUT))
            }
        };
        let outcome = match timeout {
            Some(t) => export(endpoint, signal, body, t),
            None => Err("no time left before exit".to_string()),
        };
        let stop = outcome.is_err() && deadline.is_some();
        let health = if signal == "logs" {
            &LOGS_HEALTH
        } else {
            &TRACES_HEALTH
        };
        // The verdict is taken under the lock and said after it, so a log
        // line (which the logs exporter buffers) never runs under it.
        let say = {
            let mut h = health.lock().unwrap_or_else(|e| e.into_inner());
            if stop {
                h.lost += rest as u64;
            }
            h.record(outcome.is_ok(), Instant::now())
        };
        match say {
            Say::Nothing => {}
            Say::Failed { failures } => log.warn(
                "otel.export.failed",
                json!({
                    "signal": signal,
                    "endpoint": endpoint,
                    "err": outcome.err().unwrap_or_default(),
                    "failures": failures,
                }),
            ),
            Say::Recovered { failures } => log.info(
                "otel.export.recovered",
                json!({"signal": signal, "endpoint": endpoint, "failures": failures}),
            ),
        }
        !stop
    }

    // ---- OTLP logs (optional) ----------------------------------------------
    // A bounded buffer drained by a background thread to `<endpoint>/v1/logs`.
    // The JSON-lines log surface is the primary one; this is the OTLP mirror.

    /// The armed logs exporter: where it sends, and the buffer it drains.
    struct Logs {
        endpoint: String,
        service: String,
        version: String,
        buf: Mutex<Vec<Value>>,
    }

    static LOGS: OnceLock<Logs> = OnceLock::new();

    /// Buffer one log record (no-op unless [`arm_logs`] ran; bounded at 8192 so a
    /// stalled/absent collector can never grow memory without bound). The
    /// record names the span the line is in: its `trace_id` and `span_id`
    /// are the record's `traceId` and `spanId`.
    pub(super) fn capture_log(unix_nanos: u128, level: &str, event: &str, fields: &Value) {
        let Some(logs) = LOGS.get() else { return };
        let mut b = logs.buf.lock().unwrap_or_else(|e| e.into_inner());
        if b.len() >= 8192 {
            return;
        }
        b.push(log_record(unix_nanos, level, event, fields));
    }

    /// One OTLP `LogRecord` for a line.
    pub(super) fn log_record(unix_nanos: u128, level: &str, event: &str, fields: &Value) -> Value {
        let mut rec = json!({
            "timeUnixNano": unix_nanos.to_string(),
            "severityText": level.to_ascii_uppercase(),
            "body": { "stringValue": event },
            "attributes": [ { "key": "log.fields", "value": { "stringValue": fields.to_string() } } ],
        });
        for (field, key) in [("trace_id", "traceId"), ("span_id", "spanId")] {
            if let Some(id) = fields.get(field).and_then(Value::as_str) {
                rec[key] = json!(id);
            }
        }
        rec
    }

    /// Arm the OTLP logs exporter: install the buffer + spawn a background flush
    /// thread. Idempotent (a second call no-ops).
    pub(super) fn arm_logs(log: &Logger, endpoint: &str, service: &str, version: &str) {
        let armed = Logs {
            endpoint: endpoint.to_string(),
            service: service.to_string(),
            version: version.to_string(),
            buf: Mutex::new(Vec::new()),
        };
        if LOGS.set(armed).is_err() {
            return; // already armed
        }
        let log = log.clone();
        std::thread::Builder::new()
            .name("otel-logs".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    flush_logs(&log, None);
                }
            })
            .ok();
    }

    /// Export the buffered log records, on this thread — by `deadline`, when
    /// an exit waits on it (see [`send`]).
    pub(super) fn flush_logs(log: &Logger, deadline: Option<Instant>) {
        let Some(logs) = LOGS.get() else { return };
        let batch = std::mem::take(&mut *logs.buf.lock().unwrap_or_else(|e| e.into_inner()));
        if batch.is_empty() {
            return;
        }
        let body = to_otlp_logs_json(&batch, &logs.service, &logs.version);
        send(log, &logs.endpoint, "logs", &body, deadline, 0);
    }

    pub(super) fn to_otlp_logs_json(records: &[Value], service: &str, version: &str) -> Value {
        json!({
            "resourceLogs": [{
                "resource": { "attributes": [
                    { "key": "service.name", "value": { "stringValue": service } },
                    { "key": "service.version", "value": { "stringValue": version } },
                ]},
                "scopeLogs": [{ "scope": { "name": "agentd" }, "logRecords": records }]
            }]
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

        fn span() -> Span {
            let t = TraceContext {
                trace_id: TRACE.into(),
                span_id: "00f067aa0ba902b7".into(),
                parent_span_id: None,
                flags: "01".into(),
                tracestate: None,
            };
            let mut s = Span::begin(
                &t,
                "invoke_agent",
                Kind::Internal,
                1_700_000_000_000_000_000,
            );
            s.end_unix_nanos = 1_700_000_001_000_000_000;
            s.attrs = vec![
                ("gen_ai.operation.name", Attr::from("invoke_agent")),
                ("gen_ai.usage.input_tokens", Attr::from(1234u64)),
            ];
            s
        }

        #[test]
        fn otlp_json_has_the_expected_shape() {
            let v = to_otlp_json(&[span()], "agentd", "0.1.0");
            let rs = &v["resourceSpans"][0];
            assert_eq!(
                rs["resource"]["attributes"][0]["value"]["stringValue"],
                "agentd"
            );
            let sp = &rs["scopeSpans"][0]["spans"][0];
            assert_eq!(sp["traceId"], TRACE);
            assert_eq!(sp["name"], "invoke_agent");
            assert_eq!(sp["kind"], 1); // INTERNAL
            assert_eq!(sp["status"]["code"], 1); // OK
            assert_eq!(sp["startTimeUnixNano"], "1700000000000000000"); // stringified
            assert_eq!(sp["attributes"][1]["value"]["intValue"], "1234"); // stringified int
            assert!(sp.get("parentSpanId").is_none());
            assert!(sp.get("links").is_none());
        }

        #[test]
        fn a_span_carries_its_kind_parent_state_and_links() {
            let mut s = span();
            s.ok = false;
            s.kind = Kind::Client;
            s.parent_span_id = Some("aaaaaaaaaaaaaaaa".into());
            s.tracestate = Some("k=v".into());
            s.add_link("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
            // A malformed link and a repeated one are left out.
            s.add_link("garbage");
            s.add_link("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
            let sp = to_otlp_json(&[s], "agentd", "0.1.0");
            let sp = &sp["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
            assert_eq!(sp["status"]["code"], 2); // ERROR
            assert_eq!(sp["kind"], 3); // CLIENT
            assert_eq!(sp["parentSpanId"], "aaaaaaaaaaaaaaaa");
            assert_eq!(sp["traceState"], "k=v");
            assert_eq!(
                sp["links"],
                json!([{"traceId": "0af7651916cd43dd8448eb211c80319c", "spanId": "b7ad6b7169203331"}])
            );
        }

        fn in_span(span: &str, parent: Option<&str>) -> TraceContext {
            TraceContext {
                trace_id: TRACE.into(),
                span_id: span.into(),
                parent_span_id: parent.map(str::to_string),
                flags: "01".into(),
                tracestate: None,
            }
        }

        /// The links a line names wait for the span the line is in, and go out
        /// with it — once. A link to the span's own parent is not one.
        #[test]
        fn a_line_s_links_go_out_with_its_span() {
            let mine = in_span("1111111111111111", Some("3333333333333333"));
            capture_links(
                "x",
                &mine,
                &json!({
                    "link": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                    "links": ["00-0af7651916cd43dd8448eb211c80319c-c7ad6b7169203331-01",
                              format!("00-{TRACE}-3333333333333333-01")],
                }),
            );
            // A line naming nothing holds nothing.
            capture_links("x", &in_span("2222222222222222", None), &json!({}));
            assert_eq!(end_links(&mine.span_id).len(), 2);
            assert!(end_links(&mine.span_id).is_empty());
            assert!(end_links("2222222222222222").is_empty());
        }

        /// A line about a span that has already gone out names its links on a
        /// span of its own, named for the line, under that span.
        #[test]
        fn a_link_named_after_its_span_ended_goes_out_on_a_span_of_its_own() {
            let gate = in_span("4444444444444444", None);
            queue(Span::begin(&gate, "SendMessage", Kind::Server, 1), 2, true);
            let link = "00-0af7651916cd43dd8448eb211c80319c-d7ad6b7169203331-01";
            capture_links("human.answered", &gate, &json!({"link": link}));
            assert!(end_links(&gate.span_id).is_empty(), "nothing waits");
            let q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            let own: Vec<&Span> = q
                .iter()
                .filter(|s| s.parent_span_id.as_deref() == Some(gate.span_id.as_str()))
                .collect();
            assert_eq!(own.len(), 1, "one span for the line");
            let own = own[0];
            assert_eq!(own.name, "human.answered");
            assert_eq!(own.kind, Kind::Internal);
            assert_eq!(own.trace_id, TRACE);
            assert_ne!(own.span_id, gate.span_id);
            assert_eq!(
                own.links,
                vec![(
                    "0af7651916cd43dd8448eb211c80319c".to_string(),
                    "d7ad6b7169203331".to_string()
                )]
            );
        }

        #[test]
        fn otlp_logs_json_has_the_expected_shape() {
            let rec = log_record(
                1_700_000_000_000_000_000,
                "info",
                "turn.start",
                &json!({"trace_id": TRACE, "span_id": "00f067aa0ba902b7", "turn": "t"}),
            );
            let v = to_otlp_logs_json(&[rec], "agentd", "2.0.0");
            let rl = &v["resourceLogs"][0];
            assert_eq!(
                rl["resource"]["attributes"][0]["value"]["stringValue"],
                "agentd"
            );
            let lr = &rl["scopeLogs"][0]["logRecords"][0];
            assert_eq!(lr["body"]["stringValue"], "turn.start");
            assert_eq!(lr["severityText"], "INFO");
            assert_eq!(lr["timeUnixNano"], "1700000000000000000");
            // The record names the span the line is in.
            assert_eq!(lr["traceId"], TRACE);
            assert_eq!(lr["spanId"], "00f067aa0ba902b7");
            // A line in no trace names none.
            let bare = log_record(0, "info", "x", &json!({}));
            assert!(bare.get("traceId").is_none() && bare.get("spanId").is_none());
        }

        /// An exit waits on a collector that takes the connection and never
        /// answers only as long as its deadline, and sends nothing after the
        /// batch that failed.
        #[test]
        fn an_exit_s_export_gives_up_at_its_deadline() {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", l.local_addr().unwrap());
            let held = std::thread::spawn(move || l.accept().map(|(c, _)| c));
            let log = Logger::new(
                crate::obs::log::LogCtx {
                    run_id: "r".into(),
                    agent_id: "0".into(),
                    agent_path: "0".into(),
                    comp: crate::obs::log::Comp::Supervisor,
                    pid: 0,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    trace_flags: None,
                },
                crate::obs::log::Level::Error,
            );
            let t0 = Instant::now();
            let by = t0 + Duration::from_millis(300);
            assert!(!send(&log, &endpoint, "logs", &json!({}), Some(by), 3));
            let waited = t0.elapsed();
            assert!(waited < Duration::from_secs(2), "waited {waited:?}");
            // Past the deadline, nothing is sent at all.
            assert!(!send(&log, &endpoint, "logs", &json!({}), Some(t0), 0));
            drop(held.join());
        }

        #[cfg(feature = "tls")]
        #[test]
        fn an_export_failure_names_where_it_went() {
            // Nothing listens on a port just released.
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let err = export(
                &format!("https://127.0.0.1:{port}"),
                "traces",
                &json!({}),
                EXPORT_TIMEOUT,
            )
            .unwrap_err();
            assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
            let err = export("collector:4318", "logs", &json!({}), EXPORT_TIMEOUT).unwrap_err();
            assert!(err.contains("collector:4318/v1/logs"), "{err}");
        }
    }
}
