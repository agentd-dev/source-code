// SPDX-License-Identifier: AGPL-3.0-only
//! The **workflow model**: a named DAG of steps beginning at start nodes,
//! parsed from a JSON or YAML document and validated for acyclicity,
//! reachability, `finish` reachability, dependency existence, schema
//! well-formedness, CEL compilation and the caps.
//!
//! Field checking is strict per kind: a field the kind does not declare is a
//! validation error, not something to ignore. A misspelled key is the failure
//! mode that hurts most here — the workflow parses, runs, and quietly does not
//! do the thing that key was meant to configure — so an unknown field must be
//! refused where someone is still looking at it.
//!
//! The node catalogue is one table ([`KINDS`]); the validator, the executor
//! and `--workflow-schema` all read it, so a kind cannot be documented without
//! being validated or exposed without being described.

use crate::jsonschema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The dialect this model speaks.
pub const DIALECT: u32 = 3;
/// Structural caps, enforced at validation so a pathological document is
/// refused when it is submitted rather than after it has been scheduled.
pub const MAX_STEPS: usize = 512;
pub const MAX_NESTING: usize = 4;
pub const MAX_BATCH_PARALLEL: u64 = 8;
/// Lanes a `foreach`/`batch` uses when the definition does not say.
///
/// Four: concurrent enough to be worth writing `foreach` for rather than a
/// loop, and low enough not to stampede an MCP server that never asked for the
/// traffic. A definition that knows better sets its own, up to
/// [`MAX_BATCH_PARALLEL`].
pub const DEFAULT_FAN_OUT: u64 = 4;
pub const MAX_ITERATIONS: u64 = 10_000;
pub const MAX_ID_LEN: usize = 64;

/// A step kind's metadata.
#[derive(Debug, Clone, Copy)]
pub struct KindInfo {
    pub name: &'static str,
    /// A start node (a trigger).
    pub start: bool,
    /// Kind-specific fields (besides the cross-cutting ones).
    pub fields: &'static [&'static str],
    /// Required kind-specific fields.
    pub required: &'static [&'static str],
    /// Executable in this build. A kind marked `false` still parses and
    /// validates structurally, but validation then refuses the document, so a
    /// definition can never reach the scheduler naming a kind nothing runs.
    pub implemented: bool,
    /// Has a nested body sub-DAG (`body: {steps: …}`) / branches.
    pub nested: bool,
}

const fn k(
    name: &'static str,
    start: bool,
    fields: &'static [&'static str],
    required: &'static [&'static str],
    implemented: bool,
    nested: bool,
) -> KindInfo {
    KindInfo {
        name,
        start,
        fields,
        required,
        implemented,
        nested,
    }
}

/// The node catalogue: every step kind, its start-node status, the fields it
/// accepts, the fields it requires, whether this build executes it, and
/// whether it carries a nested sub-DAG. This table is the single source the
/// validator, the schema generator and the executor all consult.
pub const KINDS: &[KindInfo] = &[
    // ---- start nodes ----
    k("once", true, &["policy", "inputs"], &[], true, false),
    k("manual", true, &["inputs"], &[], true, false),
    k(
        "loop",
        true,
        &[
            "interval",
            "delay",
            "until",
            "max_iterations",
            "backoff",
            "inputs",
        ],
        &[],
        true,
        false,
    ),
    k(
        "schedule",
        true,
        &["cron", "every", "tz", "jitter", "catch_up", "at", "inputs"],
        &[],
        true,
        false,
    ),
    k(
        "subscribe",
        true,
        &[
            "server",
            "uri",
            "debounce_ms",
            "coalesce",
            "filter",
            "deliver",
            "on_no_listener",
            "window",
            "inputs",
        ],
        &["server", "uri"],
        true,
        false,
    ),
    k(
        "stream",
        true,
        &["stream", "subject", "filter", "from", "rate", "inputs"],
        &["stream"],
        true,
        false,
    ),
    k(
        "signal",
        true,
        &["name", "filter", "deliver", "inputs"],
        &["name"],
        true,
        false,
    ),
    k(
        "event",
        true,
        &["on", "filter", "inputs"],
        &["on"],
        true,
        false,
    ),
    k(
        "a2a",
        true,
        &["command", "roles", "inputs", "schema"],
        &[],
        true,
        false,
    ),
    k(
        "webhook",
        true,
        &[
            "path",
            "methods",
            "auth",
            "parallelism",
            "on_overflow",
            "rate",
            "idempotency",
            "respond",
            "filter",
            "inputs",
            "signal",
        ],
        &["path"],
        true,
        false,
    ),
    // ---- control ----
    k(
        "switch",
        false,
        &["on", "cases", "default", "on_no_match"],
        &["on", "cases"],
        true,
        false,
    ),
    k(
        "parallel",
        false,
        &["branches", "on_error"],
        &["branches"],
        true,
        true,
    ),
    k(
        "foreach",
        false,
        &["over", "body", "batch", "collect", "on_error", "as"],
        &["over", "body"],
        true,
        true,
    ),
    k(
        "batch",
        false,
        &[
            "over", "body", "by", "size", "parallel", "rate", "collect", "on_error",
        ],
        &["over", "body"],
        true,
        true,
    ),
    k(
        "iterate",
        false,
        &["body", "while", "until", "max_iterations", "collect"],
        &["body"],
        true,
        true,
    ),
    k(
        "race",
        false,
        &["branches", "timeout", "min_success"],
        &["branches"],
        true,
        true,
    ),
    k(
        "join",
        false,
        &["handles", "timeout", "min", "partials"],
        &["handles"],
        true,
        false,
    ),
    k("subgraph", false, &["body"], &["body"], true, true),
    k(
        "workflow",
        false,
        &["name", "inputs", "mode", "start", "version", "cascade"],
        &["name"],
        true,
        false,
    ),
    k(
        "wait",
        false,
        &[
            "on",
            "server",
            "uri",
            "condition",
            "signal",
            "run",
            "subagent",
            "conversation",
            "webhook",
            "timeout",
            "on_timeout",
        ],
        &["on"],
        true,
        false,
    ),
    k("sleep", false, &["duration"], &["duration"], true, false),
    k(
        "assert",
        false,
        &["condition", "message"],
        &["condition"],
        true,
        false,
    ),
    k("fail", false, &["message", "code"], &[], true, false),
    k("noop", false, &[], &[], true, false),
    k("checkpoint", false, &["name"], &[], true, false),
    k(
        "finish",
        false,
        &["status", "output", "reason"],
        &[],
        true,
        false,
    ),
    // ---- data ----
    k(
        "assign",
        false,
        &["value", "writes", "mode"],
        &["value"],
        true,
        false,
    ),
    k(
        "transform",
        false,
        &["value", "writes", "mode"],
        &["value"],
        true,
        false,
    ),
    k(
        "map",
        false,
        &["over", "expr", "as"],
        &["over", "expr"],
        true,
        false,
    ),
    k(
        "filter",
        false,
        &["over", "expr", "as"],
        &["over", "expr"],
        true,
        false,
    ),
    k(
        "reduce",
        false,
        &["over", "expr", "initial", "as", "acc"],
        &["over", "expr"],
        true,
        false,
    ),
    k(
        "sort",
        false,
        &["over", "by", "order"],
        &["over"],
        true,
        false,
    ),
    k("dedupe", false, &["over", "by"], &["over"], true, false),
    k(
        "chunk",
        false,
        &["value", "by", "size", "overlap"],
        &["value", "size"],
        true,
        false,
    ),
    k("template", false, &["text", "value"], &[], true, false),
    k("parse", false, &["text", "format"], &["text"], true, false),
    k(
        "validate",
        false,
        &["value", "schema"],
        &["value", "schema"],
        true,
        false,
    ),
    k("memory.get", false, &["key"], &["key"], true, false),
    k(
        "memory.set",
        false,
        &["key", "value", "ttl"],
        &["key", "value"],
        true,
        false,
    ),
    k("memory.list", false, &["prefix", "limit"], &[], true, false),
    k(
        "memory.push",
        false,
        &["key", "value"],
        &["key", "value"],
        true,
        false,
    ),
    k("memory.shift", false, &["key"], &["key"], true, false),
    k("memory.pop", false, &["key"], &["key"], true, false),
    k("memory.delete", false, &["key"], &["key"], true, false),
    k(
        "artifact.create",
        false,
        &["name", "mime", "content", "from_step", "sensitive"],
        &["name"],
        true,
        false,
    ),
    k("artifact.get", false, &["id"], &["id"], true, false),
    k("artifact.delete", false, &["id"], &["id"], true, false),
    k(
        "knowledge.search",
        false,
        &["query", "top_k", "filters"],
        &["query"],
        true,
        false,
    ),
    k("knowledge.get", false, &["id", "uri"], &[], true, false),
    k(
        "search.query",
        false,
        &["query", "kind", "limit", "freshness"],
        &["query"],
        true,
        false,
    ),
    k(
        "search.fetch",
        false,
        &["url", "max_bytes"],
        &["url"],
        true,
        false,
    ),
    // ---- integration ----
    k(
        "mcp.tool",
        false,
        &["server", "tool", "args", "idempotency", "breaker", "rate"],
        &["server", "tool"],
        true,
        false,
    ),
    k(
        "mcp.resource",
        false,
        &[
            "server",
            "op",
            "uri",
            "name",
            "arguments",
            "reference",
            "argument",
        ],
        &["server", "op"],
        true,
        false,
    ),
    k("tool", false, &["name", "args"], &["name"], true, false),
    k(
        "http",
        false,
        &[
            "method",
            "url",
            "headers",
            "query",
            "body",
            "json",
            "timeout",
            "expect",
            "allow_private",
            "sign",
            "idempotency",
            "breaker",
            "rate",
        ],
        &["url"],
        true,
        false,
    ),
    k(
        "a2a.send",
        false,
        &[
            "to",
            "parts",
            "command",
            "args",
            "context",
            "timeout",
            "idempotency",
            "breaker",
            "rate",
        ],
        &["to"],
        true,
        false,
    ),
    k(
        "a2a.delegate",
        false,
        &[
            "peer",
            "objective",
            "command",
            "args",
            "output_contract",
            "timeout",
            "idempotency",
            "breaker",
            "rate",
        ],
        &["peer"],
        true,
        false,
    ),
    k(
        "a2a.wait",
        false,
        &["conversation", "timeout"],
        &[],
        true,
        false,
    ),
    k(
        "workflow.signal",
        false,
        &["name", "payload", "run"],
        &["name"],
        true,
        false,
    ),
    k(
        "workflow.wait",
        false,
        &["run", "timeout"],
        &["run"],
        true,
        false,
    ),
    k(
        "workflow.cancel",
        false,
        &["run", "reason"],
        &["run"],
        true,
        false,
    ),
    k(
        "emit",
        false,
        &[
            "note",
            "audit",
            "metric",
            "value",
            "stream",
            "subject",
            "data",
            "correlation",
        ],
        &[],
        true,
        false,
    ),
    // ---- intelligence & agents ----
    k(
        "think",
        false,
        &[
            "prompt",
            "output_schema",
            "reads",
            "check",
            "retries",
            "skills",
            "system",
        ],
        &["prompt"],
        true,
        false,
    ),
    k(
        "classify",
        false,
        &["input", "classes", "prompt", "skills"],
        &["input", "classes"],
        true,
        false,
    ),
    k(
        "extract",
        false,
        &["input", "output_schema", "prompt", "skills"],
        &["input", "output_schema"],
        true,
        false,
    ),
    k(
        "summarize",
        false,
        &["input", "length", "prompt", "skills"],
        &["input"],
        true,
        false,
    ),
    k(
        "judge",
        false,
        &["input", "rubric", "prompt", "skills"],
        &["input", "rubric"],
        true,
        false,
    ),
    k(
        "route",
        false,
        &["input", "choices", "prompt", "skills"],
        &["input", "choices"],
        true,
        false,
    ),
    k(
        "agent",
        false,
        &[
            "instruction",
            "output_contract",
            "output_schema",
            "tools",
            "servers",
            "limits",
            "context",
            "skills",
            "system",
        ],
        &["instruction"],
        true,
        false,
    ),
    // `template`/`params` instantiate a declared `subagents.templates` entry:
    // the step names a template and supplies its parameters rather than
    // spelling out the whole child, so one reviewed definition backs every
    // child that uses it.
    k(
        "subagent",
        false,
        &[
            "instruction",
            "template",
            "params",
            "mode",
            "tools",
            "servers",
            "limits",
            "priority",
            "context",
            "output_contract",
            "output_schema",
            "skills",
            "durable",
        ],
        &[],
        true,
        false,
    ),
    k(
        "human",
        false,
        &["question", "schema", "to", "timeout", "reply_uri"],
        &["question"],
        true,
        false,
    ),
];

/// Cross-cutting fields every step may carry, whatever its kind. Field
/// checking is the union of these and the kind's own list, so a name that
/// appears in neither is refused.
pub const COMMON_FIELDS: &[&str] = &[
    "kind",
    "depends_on",
    "when",
    "retry",
    "timeout",
    "on_error",
    "idempotent",
    "on_replay",
    "output_schema",
    "cache",
    "budget",
    "skills",
    "otel",
    "description",
];

/// Step kinds that are PURE data transforms: no external effect, no durable
/// write of their own, fully deterministic over the run's data. The
/// checkpoint-before-effect rule exists to stop a crash from losing or
/// repeating an effect, and these steps have none — a crash simply replays
/// them from the last checkpoint and reaches the same values. So the scheduler
/// skips the checkpoint for them, and an inline chain batches into its tick's
/// single checkpoint instead of paying a serialize-and-write per step, which
/// measures at roughly 40% of such a chain's cycles.
pub fn pure_data_kind(kind: &str) -> bool {
    matches!(
        kind,
        "assign"
            | "map"
            | "filter"
            | "reduce"
            | "sort"
            | "dedupe"
            | "chunk"
            | "parse"
            | "switch"
            | "noop"
            | "assert"
            | "validate"
    )
}

pub fn kind_info(name: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.name == name)
}

/// The kinds implemented by this build's engine.
pub fn implemented_kinds() -> Vec<&'static str> {
    KINDS
        .iter()
        .filter(|k| k.implemented)
        .map(|k| k.name)
        .collect()
}

/// `on_error` policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    #[default]
    Fail,
    Continue,
    Goto(String),
}

impl OnError {
    fn parse(v: &Value) -> Result<OnError, String> {
        match v.as_str() {
            Some("fail") => Ok(OnError::Fail),
            Some("continue") => Ok(OnError::Continue),
            Some(s) if s.starts_with("goto:") => {
                let t = s["goto:".len()..].trim();
                if t.is_empty() {
                    Err("on_error goto: needs a step id".into())
                } else {
                    Ok(OnError::Goto(t.to_string()))
                }
            }
            _ => Err("on_error must be fail | continue | goto:<step>".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnReplay {
    #[default]
    Retry,
    Skip,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Retry {
    #[serde(default)]
    pub max: u32,
    /// Backoff between attempts (ms), doubling; 0 = none.
    #[serde(default)]
    pub backoff_ms: u64,
}

/// A nested sub-DAG: the body of `foreach`/`batch`/`iterate`/`subgraph`, or one
/// branch of `parallel`/`race`. Body steps depend only on siblings; steps with
/// no dependencies are the entry points; steps nothing depends on are the
/// **sinks** whose outputs form the body's result (one sink ⇒ its output; many
/// ⇒ an object keyed by step id).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Body {
    pub steps: BTreeMap<String, Step>,
}

impl Body {
    /// Deterministic dependency order.
    pub fn topo_order(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut done: BTreeSet<String> = BTreeSet::new();
        let mut progress = true;
        while progress && out.len() < self.steps.len() {
            progress = false;
            for (id, s) in &self.steps {
                if !done.contains(id) && s.depends_on.iter().all(|d| done.contains(d)) {
                    done.insert(id.clone());
                    out.push(id.clone());
                    progress = true;
                }
            }
        }
        out
    }
    /// Steps nothing else depends on.
    pub fn sinks(&self) -> Vec<String> {
        self.steps
            .keys()
            .filter(|id| {
                !self
                    .steps
                    .values()
                    .any(|s| s.depends_on.iter().any(|d| d == *id))
            })
            .cloned()
            .collect()
    }
}

/// One step (the cross-cutting fields typed; kind fields in `spec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub on_error: OnError,
    #[serde(default)]
    pub idempotent: bool,
    #[serde(default)]
    pub on_replay: OnReplay,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The kind-specific fields, verbatim.
    #[serde(default)]
    pub spec: Map<String, Value>,
    /// The parsed nested body (`foreach`/`batch`/`iterate`/`subgraph`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
    /// The parsed branches (`parallel`/`race`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, Body>,
}

impl Step {
    pub fn info(&self) -> Option<&'static KindInfo> {
        kind_info(&self.kind)
    }
    pub fn is_start(&self) -> bool {
        self.info().is_some_and(|k| k.start)
    }
    /// A kind-specific field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.spec.get(name)
    }
    pub fn field_str(&self, name: &str) -> Option<&str> {
        self.spec.get(name).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnOverflow {
    #[default]
    Queue,
    Drop,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Concurrency {
    pub max_runs: u32,
    pub on_overflow: OnOverflow,
}

impl Default for Concurrency {
    fn default() -> Self {
        Concurrency {
            max_runs: 4,
            on_overflow: OnOverflow::Queue,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkflowLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Value>,
}

/// One declared run variable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StateDecl {
    /// A JSON Schema the written value must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// How concurrent writes combine: `overwrite | append | merge | union`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reducer: Option<String>,
}

/// A parsed, validated workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub version: u32,
    /// Scheduling weight under contention. `low` admissions shed one pressure
    /// level EARLIER (at `warn`, not just `shed`),
    /// and ready steps of higher-priority runs are scheduled first each tick.
    /// It is a tiebreak under scarcity, not a reservation.
    #[serde(default)]
    pub priority: Priority,
    /// Retirement policy for live runs (`unload: {policy, timeout}`).
    #[serde(default)]
    pub unload: Unload,
    /// Durability class: `Some(false)` ⇒ runs of this workflow are memory-only
    /// (no checkpoints, gone after a restart — the fast path for recomputable
    /// work); `Some(true)` ⇒ durable even under `store.durability.work:
    /// ephemeral`. `None` in a freshly parsed document; the loader resolves it
    /// against the store's default before the definition is armed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<bool>,
    /// Declared run variables: `{key: {type, reducer}}`.
    ///
    /// Optional, and the point is to make concurrent writes a DECLARED policy
    /// instead of a heuristic. Without it the parser can only guess from the
    /// modes two racing writers happen to use; with it, the workflow states
    /// what a key is and how writes to it combine, and disagreement is a config
    /// error rather than a value that depends on completion order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state: BTreeMap<String, StateDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub armed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs_schema: Option<Value>,
    #[serde(default)]
    pub concurrency: Concurrency,
    #[serde(default)]
    pub limits: WorkflowLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs_schema: Option<Value>,
    pub steps: BTreeMap<String, Step>,
    /// SHA-256 of the canonical definition. A run pins the hash it started
    /// against, so a redefinition never changes the shape of work already in
    /// flight, and `workflow.list` can show whether two instances agree.
    pub hash: String,
    /// The definition as given (canonical JSON), for `workflow.list`/hash.
    pub definition: Value,
}

fn default_true() -> bool {
    true
}

/// What happens to a workflow's LIVE runs when its definition goes away —
/// removed from the config, replaced by another version, or `workflow.delete`d.
/// Whatever the policy, withdrawing a definition always disarms its starts,
/// unsubscribes its MCP resources, stops admitting new runs, and pins the
/// definition each surviving run started against, so a run's shape never
/// changes underneath it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnloadPolicy {
    /// Let live runs finish (bounded by `timeout`, then cancel). The default:
    /// work that was admitted deserves to complete.
    #[default]
    Drain,
    /// Cancel live runs now.
    Cancel,
    /// Pin and forget: live runs finish whenever they finish.
    Detach,
}

impl UnloadPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            UnloadPolicy::Drain => "drain",
            UnloadPolicy::Cancel => "cancel",
            UnloadPolicy::Detach => "detach",
        }
    }
}

/// The `unload:` declaration (`{policy, timeout}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Unload {
    #[serde(default)]
    pub policy: UnloadPolicy,
    /// Drain bound in ms; `None` = unbounded (detach-like drain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Contention priority — for workflows and subagent spawns. Ordering matters:
/// higher is more important.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
        }
    }
    /// Parse from a spec value; `None` field = `Normal`, junk = `Err`.
    pub fn from_spec(v: Option<&Value>) -> Result<Priority, String> {
        match v.and_then(Value::as_str) {
            None if v.is_none() => Ok(Priority::Normal),
            Some("low") => Ok(Priority::Low),
            Some("normal") => Ok(Priority::Normal),
            Some("high") => Ok(Priority::High),
            other => Err(format!(
                "priority must be low|normal|high, got {:?}",
                other
                    .map(str::to_string)
                    .unwrap_or_else(|| v.map(|x| x.to_string()).unwrap_or_default())
            )),
        }
    }
    /// The niceness delta OS-level allocation uses (`setpriority`): `low`
    /// yields CPU (+10), `high` asks for more (−5, granted only with
    /// CAP_SYS_NICE), `normal` inherits.
    pub fn nice(self) -> Option<i32> {
        match self {
            Priority::Low => Some(10),
            Priority::Normal => None,
            Priority::High => Some(-5),
        }
    }
}

impl Workflow {
    pub fn start_steps(&self) -> Vec<&Step> {
        self.steps.values().filter(|s| s.is_start()).collect()
    }
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.get(id)
    }
    /// The steps that depend on `id`.
    pub fn dependents(&self, id: &str) -> Vec<&Step> {
        self.steps
            .values()
            .filter(|s| s.depends_on.iter().any(|d| d == id))
            .collect()
    }
    /// Whether any start node makes this workflow long-lived: one that keeps
    /// firing — a timer, a schedule, a subscription, an inbound signal, event,
    /// A2A message or stream — rather than running once and finishing.
    ///
    /// This decides daemon shape. An instance holding a long-lived workflow
    /// must not idle-exit, because the workflow's whole purpose is to still be
    /// there when its trigger arrives.
    pub fn is_long_lived(&self) -> bool {
        self.start_steps().iter().any(|s| {
            matches!(
                s.kind.as_str(),
                "loop" | "schedule" | "subscribe" | "signal" | "event" | "a2a" | "stream"
            )
        })
    }
    /// The step ids in a deterministic topological order (deps first).
    pub fn topo_order(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut done: BTreeSet<String> = BTreeSet::new();
        let mut progress = true;
        while progress && out.len() < self.steps.len() {
            progress = false;
            for (id, s) in &self.steps {
                if done.contains(id) {
                    continue;
                }
                if s.depends_on.iter().all(|d| done.contains(d)) {
                    done.insert(id.clone());
                    out.push(id.clone());
                    progress = true;
                }
            }
        }
        out
    }
}

/// A JSON value's shape, for a diagnostic that says what was written.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Parse + validate a dialect-3 document. Errors name every problem.
pub fn parse_workflow(doc: &Value) -> Result<Workflow, Vec<String>> {
    let mut errs = Vec::new();
    let Some(obj) = doc.as_object() else {
        return Err(vec!["a workflow must be an object".into()]);
    };
    const TOP: &[&str] = &[
        "name",
        "version",
        "description",
        "armed",
        "inputs",
        "concurrency",
        "limits",
        "outputs",
        "state",
        "steps",
        "file",
        "uri",
        "priority",
        "unload",
        "durable",
    ];
    for key in obj.keys() {
        if !TOP.contains(&key.as_str()) {
            errs.push(format!("unknown workflow field {key:?}"));
        }
    }
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !valid_id(&name) {
        errs.push(format!(
            "workflow name {name:?} must match [a-zA-Z_][a-zA-Z0-9_-]{{0,63}}"
        ));
    }
    let version = obj
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(DIALECT as u64) as u32;
    if version != DIALECT {
        errs.push(format!(
            "workflow {name:?}: version {version} is not dialect 3 (dialect 1/2 documents are refused — see docs/workflows.md §migration)"
        ));
    }
    if obj.contains_key("start") || obj.contains_key("nodes") {
        errs.push(format!("workflow {name:?}: `start`/`nodes` are dialect 1/2 — use `steps` with start nodes (docs/workflows.md §migration)"));
    }
    let armed = obj.get("armed").and_then(Value::as_bool).unwrap_or(true);
    let priority = match Priority::from_spec(obj.get("priority")) {
        Ok(p) => p,
        Err(e) => {
            errs.push(format!("workflow {name:?}: {e}"));
            Priority::Normal
        }
    };
    let unload = match obj.get("unload") {
        None => Unload::default(),
        Some(u) => {
            let policy = match u.get("policy").and_then(Value::as_str) {
                None | Some("drain") => UnloadPolicy::Drain,
                Some("cancel") => UnloadPolicy::Cancel,
                Some("detach") => UnloadPolicy::Detach,
                Some(o) => {
                    errs.push(format!(
                        "workflow {name:?}: unload.policy {o:?} must be drain|cancel|detach"
                    ));
                    UnloadPolicy::Drain
                }
            };
            let timeout_ms = match u.get("timeout") {
                None => None,
                Some(t) => match t.as_str().map(crate::config::parse_duration) {
                    Some(Ok(d)) => Some(d.as_millis() as u64),
                    _ => {
                        errs.push(format!(
                            "workflow {name:?}: unload.timeout must be a duration (\"60s\")"
                        ));
                        None
                    }
                },
            };
            if let Some(o) = u.as_object()
                && o.keys()
                    .any(|k| !matches!(k.as_str(), "policy" | "timeout"))
            {
                errs.push(format!(
                    "workflow {name:?}: unload takes {{policy, timeout}}"
                ));
            }
            Unload { policy, timeout_ms }
        }
    };
    let inputs_schema = match obj.get("inputs") {
        None => None,
        Some(v) => {
            let schema = v.get("schema").cloned().or_else(|| {
                v.as_object()
                    .filter(|m| m.contains_key("type") || m.contains_key("properties"))
                    .map(|_| v.clone())
            });
            match schema {
                Some(s) => {
                    if let Err(e) = jsonschema::check_schema(&s) {
                        errs.push(format!(
                            "workflow {name:?}: inputs.schema: {}",
                            e.join("; ")
                        ));
                    }
                    Some(s)
                }
                None => {
                    errs.push(format!("workflow {name:?}: inputs must be {{schema: …}}"));
                    None
                }
            }
        }
    };
    let outputs_schema = obj.get("outputs").and_then(|v| v.get("schema").cloned());
    if let Some(s) = &outputs_schema
        && let Err(e) = jsonschema::check_schema(s)
    {
        errs.push(format!(
            "workflow {name:?}: outputs.schema: {}",
            e.join("; ")
        ));
    }
    let concurrency = match obj.get("concurrency") {
        None => Concurrency::default(),
        Some(v) => Concurrency {
            max_runs: v
                .get("max_runs")
                .and_then(Value::as_u64)
                .unwrap_or(4)
                .clamp(1, 1024) as u32,
            on_overflow: match v.get("on_overflow").and_then(Value::as_str) {
                None | Some("queue") => OnOverflow::Queue,
                Some("drop") => OnOverflow::Drop,
                Some("replace") => OnOverflow::Replace,
                Some(o) => {
                    errs.push(format!("workflow {name:?}: concurrency.on_overflow {o:?} must be queue|drop|replace"));
                    OnOverflow::Queue
                }
            },
        },
    };
    let limits = match obj.get("limits") {
        None => WorkflowLimits::default(),
        Some(v) => WorkflowLimits {
            steps: v.get("steps").and_then(Value::as_u64).map(|x| x as u32),
            tokens: v.get("tokens").and_then(Value::as_u64),
            deadline_ms: match v.get("deadline") {
                None => None,
                Some(d) => match duration_ms(d) {
                    Ok(ms) => Some(ms),
                    Err(e) => {
                        errs.push(format!("workflow {name:?}: limits.deadline: {e}"));
                        None
                    }
                },
            },
            budget: v.get("budget").cloned(),
        },
    };
    // Steps.
    let mut steps: BTreeMap<String, Step> = BTreeMap::new();
    match obj.get("steps").and_then(Value::as_object) {
        None => errs.push(format!(
            "workflow {name:?}: `steps` (an object of steps) is required"
        )),
        Some(map) => {
            if map.len() > MAX_STEPS {
                errs.push(format!(
                    "workflow {name:?}: {} steps exceed the cap of {MAX_STEPS}",
                    map.len()
                ));
            }
            for (id, sv) in map {
                if let Some(step) = parse_step(&name, id, sv, 0, &mut errs) {
                    steps.insert(id.clone(), step);
                }
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    // `state` declarations: each key names a schema and/or a reducer.
    let mut state: BTreeMap<String, StateDecl> = BTreeMap::new();
    if let Some(decls) = obj.get("state") {
        match decls.as_object() {
            None => errs.push("state must be an object of {key: {schema, reducer}}".into()),
            Some(map) => {
                for (key, decl) in map {
                    let Some(d) = decl.as_object() else {
                        errs.push(format!("state {key:?}: must be an object"));
                        continue;
                    };
                    for f in d.keys() {
                        if !matches!(f.as_str(), "schema" | "reducer") {
                            errs.push(format!(
                                "state {key:?}: unknown field {f:?} (allowed: schema, reducer)"
                            ));
                        }
                    }
                    let schema = d.get("schema").cloned();
                    if let Some(sc) = &schema
                        && let Err(e) = jsonschema::check_schema(sc)
                    {
                        errs.push(format!("state {key:?}: schema: {}", e.join("; ")));
                    }
                    let reducer = d.get("reducer").and_then(Value::as_str).map(str::to_string);
                    if let Some(r) = &reducer
                        && !matches!(r.as_str(), "overwrite" | "append" | "merge" | "union")
                    {
                        errs.push(format!(
                            "state {key:?}: reducer {r:?} must be overwrite|append|merge|union"
                        ));
                    }
                    state.insert(key.clone(), StateDecl { schema, reducer });
                }
            }
        }
    }
    let durable = match obj.get("durable") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(other) => {
            errs.push(format!("workflow durable must be a boolean (got {other})"));
            None
        }
    };
    let mut wf = Workflow {
        state,
        name,
        version,
        priority,
        unload,
        durable,
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        armed,
        inputs_schema,
        concurrency,
        limits,
        outputs_schema,
        steps,
        hash: String::new(),
        definition: doc.clone(),
    };
    validate_graph(&wf, &mut errs);
    if !errs.is_empty() {
        return Err(errs);
    }
    wf.hash = crate::sha::sha256_hex(canonical(doc).as_bytes());
    Ok(wf)
}

fn parse_step(
    wf: &str,
    id: &str,
    sv: &Value,
    depth: usize,
    errs: &mut Vec<String>,
) -> Option<Step> {
    let at = format!("workflow {wf:?} step {id:?}");
    if !valid_id(id) {
        errs.push(format!(
            "{at}: id must match [a-zA-Z_][a-zA-Z0-9_-]{{0,63}}"
        ));
    }
    let Some(o) = sv.as_object() else {
        errs.push(format!("{at}: must be an object"));
        return None;
    };
    let kind = match o.get("kind").and_then(Value::as_str) {
        Some(k) => k.to_string(),
        None => {
            errs.push(format!("{at}: `kind` is required"));
            return None;
        }
    };
    let Some(info) = kind_info(&kind) else {
        errs.push(format!(
            "{at}: unknown kind {kind:?} (run `agentd --workflow-schema` for the kind catalogue)"
        ));
        return None;
    };
    // Strict fields.
    let mut spec = Map::new();
    for (key, v) in o {
        // A field the KIND declares wins over the cross-cutting list, and the
        // order matters, because `output_schema` is both. The required check
        // below looks only in `spec`, so treating it as a common field would
        // make `extract` — which declares and requires it — impossible to
        // satisfy; and the presets that merely accept it (`think`, `classify`,
        // `judge`, `route`, `summarize`) read it from `spec` at dispatch, so
        // they would silently get no schema to shape the model's answer. The
        // cross-cutting reading, which validates a step's OUTPUT, takes its
        // copy from `o` directly, so both readings still see the field.
        if info.fields.contains(&key.as_str()) {
            spec.insert(key.clone(), v.clone());
        } else if COMMON_FIELDS.contains(&key.as_str()) {
            continue;
        } else {
            errs.push(format!(
                "{at}: unknown field {key:?} for kind {kind:?} (allowed: {})",
                info.fields.join(", ")
            ));
        }
    }
    for req in info.required {
        if !spec.contains_key(*req) {
            errs.push(format!("{at}: kind {kind:?} requires field {req:?}"));
        }
    }
    if !info.implemented {
        errs.push(format!(
            "{at}: kind {kind:?} is not available in this build; implemented kinds: {}",
            implemented_kinds().join(", ")
        ));
    }
    // Nested bodies / branches: parsed into typed sub-DAGs and validated.
    let mut body: Option<Body> = None;
    let mut branches: BTreeMap<String, Body> = BTreeMap::new();
    if info.nested {
        if depth + 1 > MAX_NESTING {
            errs.push(format!("{at}: nesting exceeds {MAX_NESTING}"));
        }
        if matches!(kind.as_str(), "parallel" | "race") {
            match spec.get("branches").and_then(Value::as_object) {
                Some(bm) if !bm.is_empty() => {
                    for (bname, bv) in bm {
                        if !valid_id(bname) {
                            errs.push(format!("{at}: branch name {bname:?} must match [a-zA-Z_][a-zA-Z0-9_-]{{0,63}}"));
                        }
                        if let Some(b) =
                            parse_body(&format!("{wf}/{id}/{bname}"), bv, depth + 1, errs)
                        {
                            branches.insert(bname.clone(), b);
                        }
                    }
                }
                _ => errs.push(format!(
                    "{at}: branches must be a non-empty object of {{steps: {{…}}}} bodies"
                )),
            }
        } else {
            match spec.get("body") {
                Some(bv) => body = parse_body(&format!("{wf}/{id}"), bv, depth + 1, errs),
                None => errs.push(format!("{at}: body is required")),
            }
        }
    }
    let depends_on: Vec<String> = match o.get("depends_on") {
        None => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(_) => {
            errs.push(format!("{at}: depends_on must be a list of step ids"));
            Vec::new()
        }
    };
    if info.start && !depends_on.is_empty() {
        errs.push(format!("{at}: a start node cannot depend on other steps"));
    }
    let when = o.get("when").and_then(Value::as_str).map(str::to_string);
    if let Some(w) = &when {
        let expr = w.trim().trim_start_matches("CEL:").trim();
        if let Err(e) = crate::cel::compile_check(expr) {
            errs.push(format!("{at}: when: {e}"));
        }
    }
    let retry = o.get("retry").map(|r| Retry {
        max: r.get("max").and_then(Value::as_u64).unwrap_or(0).min(20) as u32,
        backoff_ms: match r.get("backoff") {
            None => 0,
            Some(b) => duration_ms(b).unwrap_or_else(|e| {
                errs.push(format!("{at}: retry.backoff: {e}"));
                0
            }),
        },
    });
    let timeout_ms = match o.get("timeout") {
        None => None,
        Some(t) => match duration_ms(t) {
            Ok(ms) => Some(ms),
            Err(e) => {
                errs.push(format!("{at}: timeout: {e}"));
                None
            }
        },
    };
    let on_error = match o.get("on_error") {
        None => OnError::Fail,
        Some(v) => OnError::parse(v).unwrap_or_else(|e| {
            errs.push(format!("{at}: {e}"));
            OnError::Fail
        }),
    };
    let on_replay = match o.get("on_replay").and_then(Value::as_str) {
        None | Some("retry") => OnReplay::Retry,
        Some("skip") => OnReplay::Skip,
        Some("fail") => OnReplay::Fail,
        Some(x) => {
            errs.push(format!("{at}: on_replay {x:?} must be retry|skip|fail"));
            OnReplay::Retry
        }
    };
    let output_schema = o.get("output_schema").cloned();
    if let Some(s) = &output_schema
        && let Err(e) = jsonschema::check_schema(s)
    {
        errs.push(format!("{at}: output_schema: {}", e.join("; ")));
    }
    // `idempotency` shapes. Validated per kind because the transports differ:
    // HTTP names WHERE the key travels (a header or a query parameter), the
    // others only ever override its VALUE. `true` means "the default derived
    // key", which for `mcp.tool` is already automatic.
    if let Some(idem) = spec.get("idempotency") {
        match kind.as_str() {
            "http" => {
                let ok = idem.as_object().is_some_and(|o| {
                    let hdr = o.get("header").map(|v| v.is_string());
                    let qry = o.get("query").map(|v| v.is_string());
                    let val = o.get("value").is_none_or(|v| v.is_string());
                    let known = o
                        .keys()
                        .all(|k| matches!(k.as_str(), "header" | "query" | "value"));
                    known && val && matches!((hdr, qry), (Some(true), None) | (None, Some(true)))
                });
                if !ok {
                    errs.push(format!(
                        "{at}: http idempotency takes {{header: NAME}} or {{query: NAME}} \
                         (exactly one), with an optional string value"
                    ));
                }
            }
            "mcp.tool" | "a2a.send" | "a2a.delegate" => {
                let ok = idem.is_boolean()
                    || idem.as_object().is_some_and(|o| {
                        o.keys().all(|k| k == "value")
                            && o.get("value").is_none_or(|v| v.is_string())
                    });
                if !ok {
                    errs.push(format!(
                        "{at}: idempotency takes true or {{value: \"…\"}} on this kind"
                    ));
                }
            }
            _ => {}
        }
    }
    // `breaker` — retry's cross-run sibling on the same remote-effect kinds.
    // Both fields are REQUIRED: a breaker with no threshold or no cooldown is
    // not a default anyone chose, it is a typo.
    if let Some(b) = spec.get("breaker") {
        if !matches!(
            kind.as_str(),
            "http" | "mcp.tool" | "a2a.send" | "a2a.delegate"
        ) {
            errs.push(format!(
                "{at}: breaker applies to remote-effect kinds (http, mcp.tool, a2a.send, a2a.delegate)"
            ));
        } else {
            let ok = b.as_object().is_some_and(|o| {
                o.keys()
                    .all(|k| matches!(k.as_str(), "failures" | "cooldown"))
                    && o.get("failures")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n >= 1)
                    && o.get("cooldown")
                        .and_then(Value::as_str)
                        .is_some_and(|d| crate::config::parse_duration(d).is_ok())
            });
            if !ok {
                errs.push(format!(
                    "{at}: breaker takes {{failures: N>=1, cooldown: \"60s\"}}"
                ));
            }
        }
    }
    // `rate` — outbound throttling on the same family: the step WAITS for a
    // token rather than failing, so a fan-out cannot overrun a quota. Same
    // spelling as every other rate in the config.
    if let Some(r) = spec.get("rate")
        && matches!(
            kind.as_str(),
            "http" | "mcp.tool" | "a2a.send" | "a2a.delegate"
        )
    {
        let ok = r
            .as_str()
            .is_some_and(|r| crate::supervisor::tree::parse_rate(r).is_ok());
        if !ok {
            errs.push(format!(
                "{at}: rate must be \"<burst>/<per>s\" (e.g. \"10/1s\")"
            ));
        }
    }
    // Kind-specific sanity.
    match kind.as_str() {
        // `rate: "<burst>/<per>s"` — arrival throttling, the same spelling as
        // `a2a.principals[].quotas.rate`. Checked here so a typo surfaces with
        // the other definition errors, not as a startup refusal.
        "webhook" => {
            if let Some(r) = spec.get("rate") {
                let ok = r.as_str().is_some_and(|r| {
                    r.split_once('/').is_some_and(|(b, p)| {
                        let per = p.trim();
                        let per = per
                            .strip_suffix('s')
                            .or_else(|| per.strip_suffix("sec"))
                            .unwrap_or(per);
                        b.trim().parse::<u32>().is_ok_and(|b| b > 0)
                            && per.trim().parse::<f64>().is_ok_and(|s| s > 0.0)
                    })
                });
                if !ok {
                    errs.push(format!(
                        "{at}: rate must be \"<burst>/<per>s\" (e.g. \"20/1s\")"
                    ));
                }
            }
        }
        // The typed A2A form: `command` carries the op, `args` its payload.
        "a2a.delegate" => {
            if spec.get("objective").is_none() && spec.get("command").is_none() {
                errs.push(format!(
                    "{at}: needs `objective` (prose) or `command` (typed)"
                ));
            }
            if spec.get("args").is_some() && spec.get("command").is_none() {
                errs.push(format!("{at}: `args` needs `command`"));
            }
        }
        "a2a.send" => {
            if spec.get("args").is_some() && spec.get("command").is_none() {
                errs.push(format!("{at}: `args` needs `command`"));
            }
        }
        // A subagent step needs exactly one definition — freeform prose or a
        // declared template, never both, never neither. Both would leave the
        // child's grant ambiguous; neither leaves nothing to run.
        "subagent" => {
            match (spec.get("instruction").is_some(), spec.get("template").is_some()) {
                (false, false) => errs.push(format!(
                    "{at}: needs `instruction` (freeform) or `template` (a subagents.templates entry)"
                )),
                (true, true) => errs.push(format!(
                    "{at}: `instruction` and `template` are mutually exclusive"
                )),
                _ => {}
            }
            if spec.get("params").is_some() && spec.get("template").is_none() {
                errs.push(format!("{at}: `params` needs `template`"));
            }
            for k in ["tools", "servers"] {
                if spec.get(k).is_some() && spec.get("template").is_some() {
                    errs.push(format!(
                        "{at}: `{k}` may not be combined with `template` — the template defines the grant"
                    ));
                }
            }
        }
        // `emit` publishes to a stream when `stream:` is present. The two
        // addressing fields travel together or not at all: a stream without a
        // subject has nowhere to land, and a subject without a stream names a
        // destination that does not exist.
        "emit" => {
            if spec.get("stream").is_some() != spec.get("subject").is_some() {
                errs.push(format!(
                    "{at}: a stream emit needs both `stream` and `subject`"
                ));
            }
        }
        // `stream` consumer: `from` picks the initial offset once, at arm.
        "stream" => {
            if let Some(f) = spec.get("from")
                && !matches!(f.as_str(), Some("new") | Some("earliest"))
            {
                errs.push(format!("{at}: from must be \"new\" or \"earliest\""));
            }
        }
        // `window: {samples: N}` — deliver the last N read values as an array
        // (the trend, not just the latest reading — the hardware-stream shape).
        // N is capped because the ring rides the durable start-state: every
        // sample is checkpointed, so an unbounded window would convert a fast
        // sensor into disk pressure. Past 256, aggregate at the source.
        "subscribe" => {
            if let Some(w) = spec.get("window") {
                let ok = w.as_object().is_some_and(|o| {
                    o.keys().all(|k| k == "samples")
                        && o.get("samples")
                            .and_then(Value::as_u64)
                            .is_some_and(|n| (1..=256).contains(&n))
                });
                if !ok {
                    errs.push(format!("{at}: window takes {{samples: 1..=256}}"));
                }
            }
        }
        // A `switch` routes to ONE step id per case, as a string. A list reads
        // naturally — `cases: {select: [prepare]}` — and is exactly wrong: the
        // executor asks for a string, gets an array, finds no target, falls to
        // `default`, finds an array there too, and fails the run at the moment
        // the branch is taken. That is a silent trap for whoever writes the
        // config and a confusing one for whoever debugs it, so it is refused
        // here, where the message can say what to write instead.
        "switch" => {
            if let Some(cases) = spec.get("cases").and_then(Value::as_object) {
                for (case, target) in cases {
                    if !target.is_string() {
                        errs.push(format!(
                            "{at}: switch case {case:?} must name ONE step as a string \
                             (got {}); write `{case}: some_step`, not a list",
                            json_kind(target)
                        ));
                    }
                }
            }
            if let Some(m) = spec.get("on_no_match")
                && !matches!(m.as_str(), Some("skip") | Some("fail"))
            {
                errs.push(format!("{at}: on_no_match must be \"skip\" or \"fail\""));
            }
            if let Some(d) = spec.get("default")
                && !d.is_string()
            {
                errs.push(format!(
                    "{at}: switch default must name ONE step as a string (got {}); \
                     write `default: some_step`, not a list",
                    json_kind(d)
                ));
            }
        }
        // `collect.mode` and `assign.mode` reach `write_var`, which falls through
        // to overwrite on anything it does not recognise — so `mode: appned`
        // silently overwrote instead of appending. The set is closed; check it
        // where the typo is still a config error.
        "foreach" | "batch" | "iterate" | "parallel" | "race" | "subgraph"
            if spec.contains_key("collect") =>
        {
            if let Some(m) = spec
                .get("collect")
                .and_then(|c| c.get("mode"))
                .and_then(Value::as_str)
                && !matches!(m, "overwrite" | "append" | "merge" | "union")
            {
                errs.push(format!(
                    "{at}: collect.mode {m:?} must be overwrite|append|merge|union"
                ));
            }
        }
        // `human.to` and `human.reply_uri` are accepted and then ignored — the
        // gate is answered over A2A by whoever holds the task. Rather than
        // pretend to route, refuse them: a field that silently does nothing is
        // worse than one that does not exist.
        "human" => {
            for f in ["to", "reply_uri"] {
                if spec.contains_key(f) {
                    errs.push(format!(
                        "{at}: human.{f} is not implemented — a gate is answered over A2A by \
                         whoever holds the task; remove it (see docs/node-registry.md)"
                    ));
                }
            }
        }
        "finish" => {
            if let Some(st) = spec.get("status").and_then(Value::as_str)
                && !matches!(st, "completed" | "failed" | "refused" | "cancelled")
            {
                errs.push(format!(
                    "{at}: finish.status must be completed|failed|refused|cancelled"
                ));
            }
        }
        "sleep" => {
            if let Some(d) = spec.get("duration")
                && let Err(e) = duration_ms(d)
            {
                errs.push(format!("{at}: sleep.duration: {e}"));
            }
        }
        "assert" => {
            if let Some(c) = spec.get("condition").and_then(Value::as_str)
                && let Err(e) =
                    crate::cel::compile_check(c.trim().trim_start_matches("CEL:").trim())
            {
                errs.push(format!("{at}: assert.condition: {e}"));
            }
        }
        "think" | "agent" => {
            if let Some(s) = spec.get("output_schema")
                && let Err(e) = jsonschema::check_schema(s)
            {
                errs.push(format!("{at}: output_schema: {}", e.join("; ")));
            }
        }
        "validate" => {
            if let Some(s) = spec.get("schema")
                && let Err(e) = jsonschema::check_schema(s)
            {
                errs.push(format!("{at}: schema: {}", e.join("; ")));
            }
        }
        "assign" | "transform" => {
            if let Some(m) = spec.get("mode").and_then(Value::as_str)
                && !matches!(m, "overwrite" | "append" | "merge" | "union")
            {
                errs.push(format!("{at}: mode must be overwrite|append|merge|union"));
            }
        }
        _ => {}
    }
    // Any `CEL:` valued field compiles.
    for (key, v) in &spec {
        if let Some(s) = v.as_str()
            && let Some(expr) = s.trim().strip_prefix("CEL:")
            && let Err(e) = crate::cel::compile_check(expr.trim())
        {
            errs.push(format!("{at}: {key}: {e}"));
        }
    }
    Some(Step {
        id: id.to_string(),
        kind,
        depends_on,
        when,
        retry,
        timeout_ms,
        on_error,
        idempotent: o
            .get("idempotent")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        on_replay,
        output_schema,
        cache: o.get("cache").cloned(),
        budget: o.get("budget").and_then(Value::as_u64),
        skills: o
            .get("skills")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        description: o
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        spec,
        body,
        branches,
    })
}

/// Parse + validate a nested body `{steps: {…}}`.
fn parse_body(at: &str, bv: &Value, depth: usize, errs: &mut Vec<String>) -> Option<Body> {
    let Some(bs) = bv.get("steps").and_then(Value::as_object) else {
        errs.push(format!("{at}: body must be {{steps: {{…}}}}"));
        return None;
    };
    if bs.is_empty() {
        errs.push(format!("{at}: body has no steps"));
        return None;
    }
    let mut steps = BTreeMap::new();
    for (bid, sv) in bs {
        if let Some(step) = parse_step(at, bid, sv, depth, errs) {
            if step.is_start() {
                errs.push(format!(
                    "{at} step {bid:?}: a start node cannot be inside a body"
                ));
            }
            if step.kind == "finish" {
                errs.push(format!("{at} step {bid:?}: `finish` cannot be inside a body (a body's sinks are its result)"));
            }
            steps.insert(bid.clone(), step);
        }
    }
    let body = Body { steps };
    for s in body.steps.values() {
        for d in &s.depends_on {
            if !body.steps.contains_key(d) {
                errs.push(format!(
                    "{at} step {:?}: depends_on names {d:?}, which is not a sibling in the body",
                    s.id
                ));
            }
        }
        if let OnError::Goto(t) = &s.on_error
            && !body.steps.contains_key(t)
        {
            errs.push(format!(
                "{at} step {:?}: on_error goto {t:?} is not a sibling in the body",
                s.id
            ));
        }
    }
    if body.topo_order().len() != body.steps.len() {
        errs.push(format!("{at}: cycle inside the body"));
    }
    Some(body)
}

/// Graph-level validation of declared state: a check that needs the whole DAG,
/// because it is about steps that can run *concurrently*.
///
/// Two steps that can run in the same wave, both writing one var with modes
/// that disagree, is a silent last-write-wins race: which value survives
/// depends on completion order, which is not a thing the author controls.
///
/// `append`/`merge` are reducers — several writers combining is the point.
/// `overwrite` is not: two overwriters, or an overwriter racing a reducer, is
/// the shape with no defensible answer, so it is refused where it is still a
/// config error rather than an intermittent wrong number.
fn validate_declared_state(wf: &Workflow, errs: &mut Vec<String>) {
    for s in wf.steps.values() {
        if !matches!(s.kind.as_str(), "assign" | "transform") {
            continue;
        }
        let key = s
            .spec
            .get("writes")
            .and_then(Value::as_str)
            .unwrap_or(s.id.as_str());
        let Some(decl) = wf.state.get(key) else {
            continue;
        };
        // A declared reducer is the policy for that key; a step that writes it
        // with a different mode is contradicting the declaration, which is the
        // kind of disagreement that should not survive to runtime.
        if let Some(want) = &decl.reducer {
            let mode = s
                .spec
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("overwrite");
            if mode != want {
                errs.push(format!(
                    "workflow {:?} step {:?}: writes {key:?} with mode {mode:?}, but state \
                     declares reducer {want:?}",
                    wf.name, s.id
                ));
            }
        }
    }
}

fn validate_concurrent_writes(wf: &Workflow, errs: &mut Vec<String>) {
    use std::collections::BTreeMap;
    let mut writers: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for s in wf.steps.values() {
        if !matches!(s.kind.as_str(), "assign" | "transform") {
            continue;
        }
        let key = s
            .spec
            .get("writes")
            .and_then(Value::as_str)
            .unwrap_or(s.id.as_str());
        let mode = s
            .spec
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("overwrite");
        writers.entry(key).or_default().push((s.id.as_str(), mode));
    }
    for (key, ws) in writers {
        if ws.len() < 2 {
            continue;
        }
        // Ordered pairs cannot race; only steps with no path between them can.
        for (i, (a, ma)) in ws.iter().enumerate() {
            for (b, mb) in ws.iter().skip(i + 1) {
                if reachable(wf, a, b) || reachable(wf, b, a) {
                    continue;
                }
                // Nor can two arms of the same switch: exactly one is taken, so
                // they are mutually EXCLUSIVE rather than concurrent. Ordering
                // is expressed by the routing edge here, not by `depends_on`,
                // which is why the reachability walk above cannot see it.
                if exclusive_by_switch(wf, a, b) {
                    continue;
                }
                // A declared reducer settles it: the workflow has stated how
                // writes to this key combine, which is exactly the policy the
                // heuristic below is guessing at.
                if wf
                    .state
                    .get(key)
                    .and_then(|d| d.reducer.as_deref())
                    .is_some()
                {
                    continue;
                }
                // append/merge/union are reducers — several writers combining
                // is the point. Only an overwriter has no defensible answer.
                if *ma == "overwrite" || *mb == "overwrite" {
                    errs.push(format!(
                        "workflow {:?}: steps {a:?} and {b:?} can run concurrently and both \
                         write {key:?} (modes {ma}/{mb}) — the surviving value would depend on \
                         completion order; order them with depends_on, or use append/merge",
                        wf.name
                    ));
                }
            }
        }
    }
}

/// Whether two steps are arms of one `switch` — at most one of them ever runs.
fn exclusive_by_switch(wf: &Workflow, a: &str, b: &str) -> bool {
    for s in wf.steps.values() {
        if s.kind != "switch" {
            continue;
        }
        let mut arms: Vec<&str> = s
            .spec
            .get("cases")
            .and_then(Value::as_object)
            .map(|c| c.values().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if let Some(d) = s.spec.get("default").and_then(Value::as_str) {
            arms.push(d);
        }
        // Either arm may be the step itself or an ancestor of it: a whole
        // branch hangs below one target.
        let on_arm = |x: &str| arms.iter().any(|arm| *arm == x || reachable(wf, arm, x));
        if on_arm(a) && on_arm(b) {
            return true;
        }
    }
    false
}

/// Whether `to` is reachable from `from` along `depends_on` edges.
fn reachable(wf: &Workflow, from: &str, to: &str) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![to];
    // Walk UP from `to`: it is reachable from `from` if `from` is an ancestor.
    while let Some(cur) = stack.pop() {
        if cur == from {
            return true;
        }
        if !seen.insert(cur.to_string()) {
            continue;
        }
        if let Some(s) = wf.steps.get(cur) {
            for d in &s.depends_on {
                stack.push(d.as_str());
            }
        }
    }
    false
}

/// A `human` gate inside a body that can run several copies at once.
///
/// Only ONE gate can be live per run today: the second suspended `human` has no
/// task of its own to be answered through, so it waits for a reply that can
/// never be addressed to it. Inside `foreach`/`parallel`/`batch`/`race` that is
/// not a rare shape, it is the normal one — a gate per item. Refused at load
/// until each gate carries its own identity, because failing at validation is
/// much kinder than hanging at item two.
fn validate_human_in_concurrent_bodies(wf: &Workflow, errs: &mut Vec<String>) {
    fn walk(wf_name: &str, owner: &str, body: &Body, errs: &mut Vec<String>) {
        for s in body.steps.values() {
            if s.kind == "human" {
                errs.push(format!(
                    "workflow {wf_name:?} step {:?}: a `human` gate inside {owner:?} is not \
                     supported — only one gate can be live per run, so a second item would \
                     wait forever. Gate before or after the fan-out instead.",
                    s.id
                ));
            }
            for nested in s.body.iter().chain(s.branches.values()) {
                walk(wf_name, owner, nested, errs);
            }
        }
    }
    for s in wf.steps.values() {
        if !matches!(s.kind.as_str(), "foreach" | "batch" | "parallel" | "race") {
            continue;
        }
        for body in s.body.iter().chain(s.branches.values()) {
            walk(&wf.name, &s.id, body, errs);
        }
    }
}

fn validate_graph(wf: &Workflow, errs: &mut Vec<String>) {
    validate_human_in_concurrent_bodies(wf, errs);
    validate_declared_state(wf, errs);
    validate_concurrent_writes(wf, errs);
    let name = &wf.name;
    let starts: Vec<&Step> = wf.start_steps();
    if starts.is_empty() {
        errs.push(format!("workflow {name:?}: at least one start node is required (once|manual|loop|schedule|subscribe|signal|event|a2a)"));
    }
    // Dependencies + goto targets exist.
    for s in wf.steps.values() {
        for d in &s.depends_on {
            if !wf.steps.contains_key(d) {
                errs.push(format!(
                    "workflow {name:?} step {:?}: depends_on names unknown step {d:?}",
                    s.id
                ));
            }
            if d == &s.id {
                errs.push(format!(
                    "workflow {name:?} step {:?}: depends on itself",
                    s.id
                ));
            }
        }
        if let OnError::Goto(t) = &s.on_error
            && !wf.steps.contains_key(t)
        {
            errs.push(format!(
                "workflow {name:?} step {:?}: on_error goto names unknown step {t:?}",
                s.id
            ));
        }
        if let Some(t) = s.field_str("on_timeout")
            && !wf.steps.contains_key(t)
        {
            errs.push(format!(
                "workflow {name:?} step {:?}: on_timeout names unknown step {t:?}",
                s.id
            ));
        }
    }
    // An `on_timeout` target is reached by ROUTING, not by a dependency —
    // it must not depend on the wait (a satisfied wait would then fire it
    // too), so it is exempt from the unreachable-root rule and seeds
    // reachability off the step that routes to it.
    let timeout_targets: BTreeSet<String> = wf
        .steps
        .values()
        .filter_map(|s| s.field_str("on_timeout").map(str::to_string))
        .collect();
    // A non-start step with no dependencies is an unreachable root.
    for s in wf.steps.values() {
        if !s.is_start() && s.depends_on.is_empty() && !timeout_targets.contains(&s.id) {
            errs.push(format!("workflow {name:?} step {:?}: a non-start step must depend on something (unreachable root)", s.id));
        }
    }
    // Acyclic (Kahn) + reachability from a start node.
    let order = wf.topo_order();
    if order.len() != wf.steps.len() {
        let stuck: Vec<&String> = wf.steps.keys().filter(|k| !order.contains(k)).collect();
        errs.push(format!("workflow {name:?}: cycle among steps {stuck:?}"));
    }
    let mut reachable: BTreeSet<String> = starts.iter().map(|s| s.id.clone()).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for s in wf.steps.values() {
            if !reachable.contains(&s.id)
                && !s.depends_on.is_empty()
                && s.depends_on.iter().any(|d| reachable.contains(d))
            {
                reachable.insert(s.id.clone());
                changed = true;
            }
            // Routing edges reach too.
            if reachable.contains(&s.id)
                && let Some(t) = s.field_str("on_timeout")
                && !reachable.contains(t)
            {
                reachable.insert(t.to_string());
                changed = true;
            }
        }
    }
    for s in wf.steps.values() {
        if !reachable.contains(&s.id) {
            errs.push(format!(
                "workflow {name:?} step {:?}: not reachable from any start node",
                s.id
            ));
        }
    }
    if !wf.steps.values().any(|s| s.kind == "finish") {
        errs.push(format!("workflow {name:?}: a `finish` step is required"));
    }
}

/// `[a-zA-Z_][a-zA-Z0-9_-]{0,63}`.
pub fn valid_id(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    s.len() <= MAX_ID_LEN && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Fields never rendered as templates before execution (expressions the step
/// evaluates itself, and nested definitions).
pub const RAW_FIELDS: &[(&str, &str)] = &[
    ("assert", "condition"),
    ("validate", "schema"),
    ("map", "expr"),
    ("filter", "expr"),
    ("reduce", "expr"),
    ("iterate", "while"),
    ("iterate", "until"),
    ("iterate", "body"),
    ("foreach", "body"),
    ("batch", "body"),
    ("subgraph", "body"),
    ("parallel", "branches"),
    ("race", "branches"),
    ("subscribe", "filter"),
    ("signal", "filter"),
    ("event", "filter"),
    ("wait", "condition"),
    ("think", "check"),
    ("switch", "cases"),
    ("await", "condition"),
];

pub fn is_raw_field(kind: &str, field: &str) -> bool {
    RAW_FIELDS.iter().any(|(k, f)| *k == kind && *f == field)
}

/// `Some(ms)` for a duration field, `None` when absent/invalid.
pub fn duration_ms_opt(v: &Value) -> Option<u64> {
    duration_ms(v).ok()
}

/// A duration field: `"30s"`, `"5m"`, bare seconds, or ms as `{"ms": n}`.
pub fn duration_ms(v: &Value) -> Result<u64, String> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .map(|s| s * 1000)
            .ok_or_else(|| "duration must be a non-negative number of seconds".into()),
        Value::String(s) => crate::config::parse_duration(s).map(|d| d.as_millis() as u64),
        Value::Object(o) => o
            .get("ms")
            .and_then(Value::as_u64)
            .ok_or_else(|| "duration object must be {ms: n}".into()),
        _ => Err("duration must be a string like 30s or a number of seconds".into()),
    }
}

/// Canonical JSON (sorted keys — serde_json's Map is a BTreeMap here) for hashing.
pub fn canonical(v: &Value) -> String {
    v.to_string()
}

/// The workflow JSON Schema, as `--workflow-schema` prints it. Generated from
/// [`KINDS`] rather than written by hand, so the schema and the validator can
/// never disagree about which fields a kind accepts.
pub fn workflow_schema() -> Value {
    let kinds: Vec<&str> = KINDS.iter().map(|k| k.name).collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://agentd.dev/schemas/workflow-3.json",
        "title": "agentd workflow",
        "type": "object",
        "required": ["name", "steps"],
        "properties": {
            "name": {"type": "string", "pattern": "^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$"},
            "version": {"const": 3},
            "description": {"type": "string"},
            "armed": {"type": "boolean", "default": true},
            "durable": {"type": "boolean", "description": "false = runs are memory-only (no checkpoints, gone after a restart) — the fast path for recomputable work; absent = the store.durability.work default (durable)"},
            "inputs": {"type": "object", "properties": {"schema": {"type": "object"}}},
            "outputs": {"type": "object", "properties": {"schema": {"type": "object"}}},
            "state": {"type": "object", "additionalProperties": {"type": "object",
                "additionalProperties": false,
                "properties": {
                    "schema": {"type": "object", "description": "a JSON Schema every write to this key must satisfy"},
                    "reducer": {"enum": ["overwrite", "append", "merge", "union"],
                                "description": "how concurrent writes to this key combine; declaring it makes concurrency a policy rather than a race"}}},
                "description": "declared run variables — {key: {schema, reducer}}"},
            "concurrency": {"type": "object", "properties": {"max_runs": {"type": "integer", "minimum": 1}, "on_overflow": {"enum": ["queue", "drop", "replace"]}}},
            "limits": {"type": "object", "properties": {"steps": {"type": "integer"}, "tokens": {"type": "integer"}, "deadline": {"type": "string"}, "budget": {"type": "object"}}},
            "steps": {"type": "object", "additionalProperties": {"$ref": "#/$defs/step"}, "minProperties": 1}
        },
        "$defs": {
            "step": {
                "type": "object",
                "required": ["kind"],
                "properties": {
                    "kind": {"enum": kinds},
                    "depends_on": {"type": "array", "items": {"type": "string"}},
                    "when": {"type": "string"},
                    "retry": {"type": "object", "properties": {"max": {"type": "integer"}, "backoff": {"type": "string"}}},
                    "timeout": {"type": "string"},
                    "on_error": {"type": "string"},
                    "idempotent": {"type": "boolean"},
                    "on_replay": {"enum": ["retry", "skip", "fail"]},
                    "output_schema": {"type": "object"},
                    "cache": {"type": "object"},
                    "budget": {"type": "integer"},
                    "skills": {"type": "array", "items": {"type": "string"}},
                    "otel": {"type": "object"},
                    "description": {"type": "string"}
                }
            },
            "kinds": KINDS.iter().map(|k| (k.name.to_string(), json!({"start": k.start, "fields": k.fields, "required": k.required, "implemented": k.implemented}))).collect::<BTreeMap<_, _>>()
        }
    })
}

#[cfg(test)]
mod tests {
    /// `output_schema` is both a cross-cutting step field and a field several
    /// kinds declare for themselves. The kind's reading must win. `extract`
    /// REQUIRES it and the required-field check reads `spec`, so if the
    /// common-field skip took precedence `extract` could never validate — a
    /// documented, "implemented" node impossible to use. The presets that
    /// merely accept it read it from `spec` at dispatch, so they would be
    /// handed no schema at all and would fail silently rather than loudly.
    #[test]
    fn a_kind_that_declares_output_schema_receives_it() {
        let doc = serde_json::json!({
            "name": "w",
            "steps": {
                "go": {"kind": "manual"},
                "e":  {"kind": "extract", "depends_on": ["go"], "input": "x",
                       "output_schema": {"type": "object"}},
                "t":  {"kind": "think", "depends_on": ["e"], "prompt": "p",
                       "output_schema": {"type": "object"}},
                "fin": {"kind": "finish", "depends_on": ["t"], "status": "completed"}
            }
        });
        let wf = parse_workflow(&doc)
            .unwrap_or_else(|e| panic!("extract must validate with an output_schema: {e:?}"));
        // And the kind actually RECEIVES it, which is what the executor reads.
        for id in ["e", "t"] {
            let step = wf.steps.get(id).unwrap_or_else(|| panic!("step {id}"));
            assert!(
                step.field("output_schema").is_some(),
                "{id}: the kind's own output_schema must reach the node spec"
            );
        }
    }

    use super::*;

    fn wf(doc: Value) -> Result<Workflow, Vec<String>> {
        parse_workflow(&doc)
    }

    #[test]
    fn workflow_priority_parses_and_rejects_junk() {
        let w = wf(json!({"name": "w", "priority": "low", "steps": {
            "s": {"kind": "once"}, "f": {"kind": "finish", "depends_on": ["s"]}}}))
        .unwrap();
        assert_eq!(w.priority, Priority::Low);
        let w = wf(json!({"name": "w", "steps": {
            "s": {"kind": "once"}, "f": {"kind": "finish", "depends_on": ["s"]}}}))
        .unwrap();
        assert_eq!(w.priority, Priority::Normal, "default");
        let e = wf(json!({"name": "w", "priority": "urgent", "steps": {
            "s": {"kind": "once"}, "f": {"kind": "finish", "depends_on": ["s"]}}}))
        .unwrap_err();
        assert!(e.iter().any(|m| m.contains("low|normal|high")), "{e:?}");
        // Priority orders: High > Normal > Low (schedule sort relies on it).
        assert!(Priority::High > Priority::Normal && Priority::Normal > Priority::Low);
    }

    #[test]
    fn breaker_validates_shape_and_kind_family() {
        let ok = wf(json!({"name": "w", "steps": {
            "s": {"kind": "once"},
            "c": {"kind": "http", "depends_on": ["s"], "url": "https://api.example",
                  "breaker": {"failures": 5, "cooldown": "60s"}},
            "f": {"kind": "finish", "depends_on": ["c"]},
        }}));
        assert!(ok.is_ok(), "{ok:?}");
        for bad in [
            json!({"failures": 0, "cooldown": "60s"}),
            json!({"failures": 5}),
            json!({"cooldown": "60s"}),
            json!({"failures": 5, "cooldown": "sometimes"}),
            json!({"failures": 5, "cooldown": "60s", "extra": 1}),
        ] {
            let e = wf(json!({"name": "w", "steps": {
                "s": {"kind": "once"},
                "c": {"kind": "http", "depends_on": ["s"], "url": "https://x", "breaker": bad},
                "f": {"kind": "finish", "depends_on": ["c"]},
            }}))
            .unwrap_err();
            assert!(e.iter().any(|m| m.contains("breaker takes")), "{e:?}");
        }
        // A breaker on a LOCAL kind is a category error, refused loudly.
        let e = wf(json!({"name": "w", "steps": {
            "s": {"kind": "once"},
            "a": {"kind": "assign", "depends_on": ["s"], "value": 1,
                  "breaker": {"failures": 5, "cooldown": "60s"}},
            "f": {"kind": "finish", "depends_on": ["a"]},
        }}))
        .unwrap_err();
        assert!(
            e.iter()
                .any(|m| m.contains("unknown field") || m.contains("remote-effect")),
            "{e:?}"
        );
    }

    #[test]
    fn webhook_rate_and_subscribe_window_validate_their_shapes() {
        // Well-formed: both parse.
        let ok = wf(json!({"name": "w", "steps": {
            "h": {"kind": "webhook", "path": "/x", "rate": "20/1s"},
            "s": {"kind": "subscribe", "server": "m", "uri": "u://v", "window": {"samples": 64}},
            "f": {"kind": "finish", "depends_on": ["h", "s"]},
        }}));
        assert!(ok.is_ok(), "{ok:?}");
        // A malformed rate is a definition error, not a startup surprise.
        for bad in ["fast", "0/1s", "5/0s", "5"] {
            let e = wf(json!({"name": "w", "steps": {
                "h": {"kind": "webhook", "path": "/x", "rate": bad},
                "f": {"kind": "finish", "depends_on": ["h"]},
            }}))
            .unwrap_err();
            assert!(
                e.iter().any(|m| m.contains("rate must be")),
                "rate {bad:?}: {e:?}"
            );
        }
        // window: bounded, object-shaped, samples-only.
        for bad in [
            json!(64),
            json!({"samples": 0}),
            json!({"samples": 300}),
            json!({"samples": 4, "mean": true}),
        ] {
            let e = wf(json!({"name": "w", "steps": {
                "s": {"kind": "subscribe", "server": "m", "uri": "u://v", "window": bad},
                "f": {"kind": "finish", "depends_on": ["s"]},
            }}))
            .unwrap_err();
            assert!(
                e.iter().any(|m| m.contains("window takes")),
                "window {bad:?}: {e:?}"
            );
        }
    }

    #[test]
    fn the_sugar_workflow_parses_hashes_and_orders() {
        let w = wf(json!({
            "name": "main", "version": 3,
            "steps": {
                "start": {"kind": "once"},
                "work": {"kind": "agent", "depends_on": ["start"], "instruction": "{{env.instruction}}"},
                "done": {"kind": "finish", "depends_on": ["work"], "status": "completed", "output": "{{steps.work.output}}"}
            }
        }))
        .unwrap();
        assert_eq!(w.start_steps().len(), 1);
        assert_eq!(w.topo_order(), vec!["start", "work", "done"]);
        assert_eq!(w.hash.len(), 64);
        assert!(!w.is_long_lived());
        assert!(w.armed);
        assert_eq!(
            w.step("work").unwrap().field_str("instruction"),
            Some("{{env.instruction}}")
        );
        // Same definition, same hash; a changed one differs.
        let w2 = wf(w.definition.clone()).unwrap();
        assert_eq!(w2.hash, w.hash);
        let mut d = w.definition.clone();
        d["steps"]["work"]["instruction"] = json!("other");
        assert_ne!(wf(d).unwrap().hash, w.hash);
    }

    // Asserts a `when: CEL parse` diagnostic, so it needs the `cel` feature.
    #[cfg(feature = "cel")]
    #[test]
    fn validation_catches_the_parse_and_graph_level_failures() {
        // Parse-level failures (reported together, before graph checks).
        let e = wf(json!({"name": "bad name", "start": "x", "steps": {
            "a": {"kind": "agent", "instruction": "x"},
            "b": {"kind": "tool", "name": "memory.get", "depends_on": ["a"], "bogus": 1},
            "c": {"kind": "foreach", "over": "{{x}}", "body": {"steps": {"i": {"kind": "noop", "depends_on": ["q"]}, "bad id": {"kind": "noop"}}}, "depends_on": ["b"]},
            "d": {"kind": "nope", "depends_on": ["a"]},
            "e": {"kind": "sleep", "duration": "5 parsecs", "depends_on": ["a"], "when": "CEL: 1 +"},
            "s": {"kind": "once", "depends_on": ["a"]}
        }}))
        .unwrap_err();
        let joined = e.join("\n");
        for needle in [
            "workflow name \"bad name\"",
            "`start`/`nodes` are dialect 1/2",
            "unknown field \"bogus\"",
            "unknown kind \"nope\"",
            "sleep.duration",
            "when: CEL parse",
            "a start node cannot depend on other steps",
            "step \"bad id\": id must match",
        ] {
            assert!(joined.contains(needle), "missing {needle:?} in:\n{joined}");
        }
        // Graph-level failures.
        let e = wf(json!({"name": "g", "steps": {
            "s": {"kind": "once"},
            "b": {"kind": "noop", "depends_on": ["s", "zz"]},
            "e": {"kind": "sleep", "duration": "1s", "depends_on": ["s"], "on_error": "goto:nowhere"},
            "loop1": {"kind": "noop", "depends_on": ["loop2"]},
            "loop2": {"kind": "noop", "depends_on": ["loop1"]},
            "f": {"kind": "finish", "depends_on": ["b"]}
        }}))
        .unwrap_err();
        let joined = e.join("\n");
        for needle in [
            "depends_on names unknown step \"zz\"",
            "on_error goto names unknown step \"nowhere\"",
            "cycle among steps",
            "not reachable from any start node",
        ] {
            assert!(joined.contains(needle), "missing {needle:?} in:\n{joined}");
        }
        // Structural: no start, unreachable, cycle, no finish.
        let e = wf(json!({"name": "w", "steps": {
            "a": {"kind": "noop"},
            "b": {"kind": "noop", "depends_on": ["c"]},
            "c": {"kind": "noop", "depends_on": ["b"]}
        }}))
        .unwrap_err();
        let joined = e.join("\n");
        assert!(joined.contains("at least one start node"), "{joined}");
        assert!(joined.contains("unreachable root"), "{joined}");
        assert!(joined.contains("cycle among steps"), "{joined}");
        assert!(joined.contains("`finish` step is required"), "{joined}");
        // Version.
        let e = wf(json!({"name": "w", "version": 2, "steps": {"s": {"kind": "once"}, "f": {"kind": "finish", "depends_on": ["s"]}}})).unwrap_err();
        assert!(e[0].contains("not dialect 3"));
        // Happy path with every implemented kind referenced.
        let ok = wf(json!({"name": "w", "inputs": {"schema": {"type": "object"}}, "concurrency": {"max_runs": 2, "on_overflow": "drop"}, "limits": {"deadline": "10m", "steps": 50}, "steps": {
            "s": {"kind": "manual"},
            "t": {"kind": "mcp.tool", "server": "fs", "tool": "read", "args": {"path": "/x"}, "depends_on": ["s"], "retry": {"max": 2, "backoff": "1s"}, "timeout": "30s", "on_error": "continue"},
            "v": {"kind": "assign", "value": {"a": 1}, "writes": "x", "depends_on": ["t"], "when": "CEL: true"},
            "th": {"kind": "think", "prompt": "p", "output_schema": {"type": "object"}, "depends_on": ["v"]},
            "z": {"kind": "sleep", "duration": "1s", "depends_on": ["th"]},
            "f": {"kind": "finish", "depends_on": ["z"], "status": "completed", "output": "{{vars.x}}"}
        }}))
        .unwrap();
        assert_eq!(ok.concurrency.on_overflow, OnOverflow::Drop);
        assert_eq!(ok.limits.deadline_ms, Some(600_000));
        assert_eq!(
            ok.step("t").unwrap().retry.as_ref().unwrap().backoff_ms,
            1000
        );
        assert_eq!(ok.step("t").unwrap().on_error, OnError::Continue);
        assert_eq!(ok.step("t").unwrap().timeout_ms, Some(30_000));
        assert!(implemented_kinds().contains(&"agent"));
        assert!(workflow_schema()["$defs"]["kinds"]["a2a.send"]["implemented"] == json!(true));
        assert!(workflow_schema()["$defs"]["kinds"]["foreach"]["implemented"] == json!(true));
    }
}
