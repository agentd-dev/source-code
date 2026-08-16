// SPDX-License-Identifier: Apache-2.0
//! The **dialect-3 workflow model** (RFC 0027 §2–§5, §8): a named DAG of steps
//! beginning at start nodes, parsed from a JSON/YAML document with a strict
//! per-kind field check (unknown fields are refused — the RFC 0021 §4.1 typo
//! shield carried over), validated for acyclicity, reachability, `finish`
//! reachability, dependency existence, schema well-formedness, CEL
//! compilation and the caps. The node catalogue is one table ([`KINDS`]) —
//! the validator, the executor and `--workflow-schema` all read it.

use crate::jsonschema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The dialect this model speaks.
pub const DIALECT: u32 = 3;
/// Caps (RFC 0027 §8).
pub const MAX_STEPS: usize = 512;
pub const MAX_NESTING: usize = 4;
pub const MAX_BATCH_PARALLEL: u64 = 8;
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
    /// Executable in this build (`false` = parses/validates but the run
    /// engine refuses it: it lands in a later phase).
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

/// The node catalogue (RFC 0027 §4–§5). `implemented` marks the P3 executor
/// subset; the rest arrives with the P4 engine.
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
            "claim",
            "shard",
            "deliver",
            "on_no_listener",
            "inputs",
        ],
        &["server", "uri"],
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
        &["command", "roles", "inputs"],
        &[],
        false,
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
            "idempotency",
            "respond",
            "filter",
            "inputs",
        ],
        &["path"],
        true,
        false,
    ),
    // ---- control ----
    k(
        "switch",
        false,
        &["on", "cases", "default"],
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
        &["server", "tool", "args"],
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
        ],
        &["url"],
        true,
        false,
    ),
    k(
        "a2a.send",
        false,
        &["to", "parts", "context"],
        &["to"],
        false,
        false,
    ),
    k(
        "a2a.delegate",
        false,
        &["peer", "objective", "output_contract", "timeout"],
        &["peer", "objective"],
        true,
        false,
    ),
    k(
        "a2a.wait",
        false,
        &["conversation", "timeout"],
        &[],
        false,
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
        &["note", "audit", "metric", "value"],
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
    k(
        "subagent",
        false,
        &[
            "instruction",
            "mode",
            "workflow",
            "tools",
            "servers",
            "limits",
            "context",
            "output_contract",
            "output_schema",
            "skills",
        ],
        &["instruction"],
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

/// Cross-cutting fields every step may carry (RFC 0027 §5).
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

/// A parsed, validated workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub version: u32,
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
    /// SHA-256 of the canonical definition (RFC 0027 §9).
    pub hash: String,
    /// The definition as given (canonical JSON), for `workflow.list`/hash.
    pub definition: Value,
}

fn default_true() -> bool {
    true
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
    /// The start nodes considered long-lived (RFC 0030 §5 durability rule).
    pub fn is_long_lived(&self) -> bool {
        self.start_steps().iter().any(|s| {
            matches!(
                s.kind.as_str(),
                "loop" | "schedule" | "subscribe" | "signal" | "event" | "a2a"
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
        "steps",
        "file",
        "uri",
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
    let mut wf = Workflow {
        name,
        version,
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
            "{at}: unknown kind {kind:?} (see the RFC 0027 §5 catalogue)"
        ));
        return None;
    };
    // Strict fields.
    let mut spec = Map::new();
    for (key, v) in o {
        if COMMON_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if info.fields.contains(&key.as_str()) {
            spec.insert(key.clone(), v.clone());
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
        errs.push(format!("{at}: kind {kind:?} is not available in this build yet (it lands with the P4 engine); implemented kinds: {}", implemented_kinds().join(", ")));
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
    // Kind-specific sanity.
    match kind.as_str() {
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

/// Graph-level validation (RFC 0027 §8).
fn validate_graph(wf: &Workflow, errs: &mut Vec<String>) {
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
    }
    // A non-start step with no dependencies is an unreachable root.
    for s in wf.steps.values() {
        if !s.is_start() && s.depends_on.is_empty() {
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

/// The dialect-3 JSON Schema (`--workflow-schema`).
pub fn workflow_schema() -> Value {
    let kinds: Vec<&str> = KINDS.iter().map(|k| k.name).collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://agentd.dev/schemas/workflow-3.json",
        "title": "agentd workflow (dialect 3)",
        "type": "object",
        "required": ["name", "steps"],
        "properties": {
            "name": {"type": "string", "pattern": "^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$"},
            "version": {"const": 3},
            "description": {"type": "string"},
            "armed": {"type": "boolean", "default": true},
            "inputs": {"type": "object", "properties": {"schema": {"type": "object"}}},
            "outputs": {"type": "object", "properties": {"schema": {"type": "object"}}},
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
    use super::*;

    fn wf(doc: Value) -> Result<Workflow, Vec<String>> {
        parse_workflow(&doc)
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
    fn validation_catches_the_rfc_0027_section_8_failures() {
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
        assert!(workflow_schema()["$defs"]["kinds"]["a2a.send"]["implemented"] == json!(false));
        assert!(workflow_schema()["$defs"]["kinds"]["foreach"]["implemented"] == json!(true));
    }
}
