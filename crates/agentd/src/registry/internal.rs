// SPDX-License-Identifier: AGPL-3.0-only
//! The **internal tool contracts**: name, description, input and output JSON
//! Schemas, whether a built-in implementation exists (mapping-only contracts
//! are `code.run`, `knowledge.*`, `search.*`), and the default grants.
//!
//! The contract is what callers see, and an override swaps only the
//! implementation behind it. That separation is what lets an operator move a
//! tool onto an MCP server without any caller — model, workflow or subagent —
//! having to be told, and it is why the schemas here are the authority on a
//! tool's shape rather than whatever a mapped server happens to advertise.

use serde_json::{Value, json};

/// Who may call a tool by default, before configuration widens or narrows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultGrant {
    pub root: bool,
    pub workflows: bool,
    pub subagents: bool,
    /// Granted to A2A `user` principals by default.
    pub user: bool,
    /// Granted to A2A `agent` principals by default.
    pub agent: bool,
}

const ALL: DefaultGrant = DefaultGrant {
    root: true,
    workflows: true,
    subagents: true,
    user: false,
    agent: false,
};
const ROOT_WF: DefaultGrant = DefaultGrant {
    root: true,
    workflows: true,
    subagents: false,
    user: false,
    agent: false,
};
const ROOT_ONLY: DefaultGrant = DefaultGrant {
    root: true,
    workflows: false,
    subagents: false,
    user: false,
    agent: false,
};

/// One contract.
#[derive(Debug, Clone)]
pub struct Contract {
    pub name: &'static str,
    pub description: &'static str,
    pub input: Value,
    pub output: Value,
    pub builtin: bool,
    pub grant: DefaultGrant,
    /// The tool's family (`memory`, `plan`, …) for `agent.tools.internal` lists.
    pub family: &'static str,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}
fn open_obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}
fn any() -> Value {
    json!({})
}
fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn arr(items: Value) -> Value {
    json!({"type": "array", "items": items})
}

/// Every internal contract (deterministic order).
pub fn contracts() -> Vec<Contract> {
    let mut v = Vec::new();
    let mut c = |name: &'static str,
                 family: &'static str,
                 description: &'static str,
                 input: Value,
                 output: Value,
                 builtin: bool,
                 grant: DefaultGrant| {
        v.push(Contract {
            name,
            description,
            input,
            output,
            builtin,
            grant,
            family,
        });
    };

    // ---- instruction ----
    c(
        "instruction.read",
        "instruction",
        "Read the agent's current instruction (the brief it operates under).",
        obj(json!({}), &[]),
        open_obj(
            json!({"text": {"type": "string"}, "source": {"enum": ["static", "resource"]}, "uri": {"type": "string"}, "version": {"type": "string"}}),
            &["text", "source"],
        ),
        true,
        ALL,
    );
    c(
        "instruction.subscribe",
        "instruction",
        "(Re)subscribe to the instruction resource, or switch to another URI; an update re-reads it and wakes the agent.",
        obj(
            json!({"uri": s("The resource URI (omit to re-subscribe to the current one)")}),
            &[],
        ),
        open_obj(
            json!({"subscribed": {"type": "boolean"}, "uri": {"type": "string"}}),
            &["subscribed"],
        ),
        true,
        ROOT_ONLY,
    );

    // ---- subagents ----
    c(
        "subagent.run",
        "subagent",
        "Spawn a subagent: freeform `instruction`, or `template` naming a declared subagents.templates entry (fill its declared `params` only — an instance-tier template brings its own workflows and runs as a peer daemon). mode: sync (wait for the result), async (get a handle), detached (fire and forget), warm (stays alive; send it messages).",
        obj(
            json!({
                "instruction": s("The subagent's brief (freeform; mutually exclusive with template)"),
                "template": s("A subagents.templates entry to instantiate"),
                "params": {"type": "object", "description": "Values for the template's declared params (schema-validated)"},
                "mode": {"enum": ["sync", "async", "detached", "warm"], "default": "sync"},
                "tools": arr(json!({"type": "string"})),
                "servers": arr(json!({"type": "string"})),
                "limits": open_obj(json!({"steps": {"type": "integer"}, "tokens": {"type": "integer"}, "deadline": {"type": "string"}, "memory": s("OS memory cap for the child process, e.g. \"512MB\" (RLIMIT_AS)"), "cpu": s("OS CPU-time cap, e.g. \"5m\" (RLIMIT_CPU)")}), &[]),
                "priority": {"enum": ["low", "normal", "high"], "default": "normal", "description": "Contention priority: low sheds first under pressure and runs nicer; high schedules first (and asks the OS for more, best-effort)."},
                "context": arr(open_obj(json!({"role": {"type": "string"}, "content": {"type": "string"}}), &["role", "content"])),
                "output_contract": s("What the result must look like"),
                "output_schema": {"type": "object"},
                "skills": arr(json!({"type": "string"})),
                "durable": {"type": "boolean", "description": "false = a memory-only record: never persisted, never restore-respawned (the fast path for throwaway workers); absent = the store.durability.work default"}
            }),
            &[],
        ),
        open_obj(
            json!({"handle": {"type": "string"}, "status": {"type": "string"}, "result": any()}),
            &["handle", "status"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.retire",
        "subagent",
        "Begin graceful retirement of an instance-tier child: it drains its own runs and exits cleanly; escalation to SIGKILL only after the drain window.",
        obj(
            json!({"handle": s("The instance child's handle")}),
            &["handle"],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "handle": {"type": "string"}, "status": {"type": "string"}}),
            &["ok"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.send",
        "subagent",
        "Send a message into a warm subagent (steer it).",
        obj(
            json!({"handle": s("The subagent handle"), "message": s("The message")}),
            &["handle", "message"],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "handle": {"type": "string"}}),
            &["ok"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.kill",
        "subagent",
        "Cancel and stop a subagent.",
        obj(
            json!({"handle": s("The subagent handle"), "reason": s("Why")}),
            &["handle"],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "handle": {"type": "string"}}),
            &["ok"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.status",
        "subagent",
        "The status (and result, when finished) of a subagent.",
        obj(json!({"handle": s("The subagent handle")}), &["handle"]),
        open_obj(
            json!({"handle": {"type": "string"}, "status": {"type": "string"}, "mode": {"type": "string"}, "result": any(), "error": {"type": "string"}}),
            &["handle", "status"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.await",
        "subagent",
        "Wait for an async subagent to finish (bounded by timeout) and return its result.",
        obj(
            json!({"handle": s("The subagent handle"), "timeout": s("Duration, e.g. 30s")}),
            &["handle"],
        ),
        open_obj(
            json!({"handle": {"type": "string"}, "status": {"type": "string"}, "result": any(), "error": {"type": "string"}}),
            &["handle", "status"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "subagent.list",
        "subagent",
        "List the subagents of this instance.",
        obj(json!({}), &[]),
        open_obj(
            json!({"subagents": arr(json!({"type": "object"}))}),
            &["subagents"],
        ),
        true,
        ROOT_WF,
    );

    // ---- code (mapping-only) ----
    c(
        "code.run",
        "code",
        "Run code in a sandbox (only available when mapped to a sandbox MCP server).",
        obj(
            json!({"language": s("e.g. python, bash"), "code": s("The program"), "files": {"type": "object"}, "timeout": s("Duration")}),
            &["language", "code"],
        ),
        open_obj(
            json!({"stdout": {"type": "string"}, "stderr": {"type": "string"}, "exit_code": {"type": "integer"}, "files": {"type": "object"}}),
            &[],
        ),
        false,
        ROOT_WF,
    );

    // ---- memory ----
    c(
        "memory.get",
        "memory",
        "Read a value from the agent's durable memory.",
        obj(json!({"key": s("The key")}), &["key"]),
        open_obj(
            json!({"found": {"type": "boolean"}, "key": {"type": "string"}, "value": any(), "meta": {"type": "object"}}),
            &["found"],
        ),
        true,
        ALL,
    );
    c(
        "memory.set",
        "memory",
        "Write a JSON value to the agent's durable memory (optional TTL).",
        obj(
            json!({"key": s("The key"), "value": any(), "ttl": s("Duration after which the value expires")}),
            &["key", "value"],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "key": {"type": "string"}, "meta": {"type": "object"}}),
            &["ok"],
        ),
        true,
        ALL,
    );
    c(
        "memory.list",
        "memory",
        "List memory keys (optionally by prefix).",
        obj(
            json!({"prefix": s("Key prefix"), "limit": {"type": "integer", "minimum": 1}}),
            &[],
        ),
        open_obj(
            json!({"keys": arr(json!({"type": "object"})), "truncated": {"type": "boolean"}}),
            &["keys"],
        ),
        true,
        ALL,
    );
    c(
        "memory.push",
        "memory",
        "Append a value to the ARRAY at a memory key (created if absent) — the durable queue primitive.",
        obj(
            json!({"key": s("The key"), "value": any()}),
            &["key", "value"],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "key": {"type": "string"}, "length": {"type": "integer"}}),
            &["ok"],
        ),
        true,
        ALL,
    );
    c(
        "memory.shift",
        "memory",
        "Remove and return the FIRST element of the array at a memory key ({found: false} on empty).",
        obj(json!({"key": s("The key")}), &["key"]),
        open_obj(
            json!({"found": {"type": "boolean"}, "value": any(), "remaining": {"type": "integer"}}),
            &["found"],
        ),
        true,
        ALL,
    );
    c(
        "memory.pop",
        "memory",
        "Remove and return the LAST element of the array at a memory key ({found: false} on empty).",
        obj(json!({"key": s("The key")}), &["key"]),
        open_obj(
            json!({"found": {"type": "boolean"}, "value": any(), "remaining": {"type": "integer"}}),
            &["found"],
        ),
        true,
        ALL,
    );
    c(
        "memory.delete",
        "memory",
        "Delete a memory key.",
        obj(json!({"key": s("The key")}), &["key"]),
        open_obj(
            json!({"ok": {"type": "boolean"}, "key": {"type": "string"}}),
            &["ok"],
        ),
        true,
        ALL,
    );

    // ---- artifacts ----
    c(
        "artifact.create",
        "artifact",
        "Create an artifact (a named piece of content delivered with the task).",
        obj(
            json!({"name": s("File-like name"), "mime": s("MIME type, default text/plain"), "content": any(), "from_step": s("Take the content from a workflow step output"), "sensitive": {"type": "boolean"}}),
            &["name"],
        ),
        open_obj(
            json!({"id": {"type": "string"}, "name": {"type": "string"}, "size": {"type": "integer"}, "sha256": {"type": "string"}}),
            &["id"],
        ),
        true,
        ALL,
    );
    c(
        "artifact.get",
        "artifact",
        "Read an artifact by id.",
        obj(json!({"id": s("Artifact id")}), &["id"]),
        open_obj(
            json!({"id": {"type": "string"}, "name": {"type": "string"}, "mime": {"type": "string"}, "content": any(), "size": {"type": "integer"}, "sha256": {"type": "string"}}),
            &["id"],
        ),
        true,
        ALL,
    );
    c(
        "artifact.delete",
        "artifact",
        "Delete an artifact.",
        obj(json!({"id": s("Artifact id")}), &["id"]),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        ALL,
    );
    c(
        "artifact.list",
        "artifact",
        "List artifacts.",
        obj(
            json!({"prefix": s("Name prefix"), "limit": {"type": "integer"}}),
            &[],
        ),
        open_obj(
            json!({"artifacts": arr(json!({"type": "object"}))}),
            &["artifacts"],
        ),
        true,
        ALL,
    );

    // ---- conversations ----
    // Delivering into a context is how a subagent or a workflow hands work UP
    // to the agent, rather than only receiving it. Granted to workflows and
    // subagents as well as root: a child reporting something worth thinking
    // about is the ordinary case, and the hop cap — not the grant — is what
    // keeps it from looping.
    c(
        "message.send",
        "message",
        "Deliver a message into one of this agent's own conversations, starting a turn there. `to` is a context id, \"root\", or \"new\". Returns once the delivery is durable — the turn runs on its own schedule. To wait for the answer, use the `message` workflow node with `wait: reply`.",
        obj(
            json!({"to": s("Context id, \"root\", or \"new\" (default: root)"), "text": s("The message")}),
            &["text"],
        ),
        open_obj(
            json!({"delivered": {"type": "boolean"}, "conversation": {"type": "string"}, "depth": {"type": "integer"}}),
            &["delivered", "conversation"],
        ),
        true,
        ALL,
    );

    // ---- workflows ----
    c(
        "workflow.run",
        "workflow",
        "Start a run of a named workflow (with inputs).",
        obj(
            json!({"name": s("Workflow name"), "inputs": {"type": "object"}, "start": s("Which start node to fire (default: manual/once)"), "wait": {"type": "boolean", "description": "Wait for the run to finish and return its output"}, "timeout": s("Duration when waiting")}),
            &["name"],
        ),
        open_obj(
            json!({"run": {"type": "string"}, "status": {"type": "string"}, "output": any(), "task": {"type": "string"}}),
            &["run", "status"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "workflow.create",
        "workflow",
        "Define a new workflow at runtime.",
        obj(
            json!({"definition": {"type": "object"}, "arm": {"type": "boolean"}}),
            &["definition"],
        ),
        open_obj(
            json!({"name": {"type": "string"}, "hash": {"type": "string"}, "armed": {"type": "boolean"}}),
            &["name"],
        ),
        true,
        ROOT_ONLY,
    );
    c(
        "workflow.update",
        "workflow",
        "Replace a workflow definition (live runs keep their pinned hash).",
        obj(
            json!({"name": s("Workflow name"), "definition": {"type": "object"}}),
            &["name", "definition"],
        ),
        open_obj(
            json!({"name": {"type": "string"}, "hash": {"type": "string"}}),
            &["name"],
        ),
        true,
        ROOT_ONLY,
    );
    c(
        "workflow.delete",
        "workflow",
        "Delete a workflow definition (disarms it; live runs finish).",
        obj(json!({"name": s("Workflow name")}), &["name"]),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        ROOT_ONLY,
    );
    c(
        "workflow.list",
        "workflow",
        "List workflows and their runs.",
        obj(json!({}), &[]),
        open_obj(
            json!({"workflows": arr(json!({"type": "object"}))}),
            &["workflows"],
        ),
        true,
        ALL,
    );
    c(
        "workflow.status",
        "workflow",
        "The status of a run (or of every run of a workflow).",
        obj(json!({"run": s("Run id"), "name": s("Workflow name")}), &[]),
        open_obj(json!({"runs": arr(json!({"type": "object"}))}), &["runs"]),
        true,
        ALL,
    );
    c(
        "workflow.cancel",
        "workflow",
        "Cancel a run.",
        obj(json!({"run": s("Run id"), "reason": s("Why")}), &["run"]),
        open_obj(
            json!({"ok": {"type": "boolean"}, "status": {"type": "string"}}),
            &["ok"],
        ),
        true,
        ROOT_WF,
    );
    c(
        "workflow.pause",
        "workflow",
        "Pause a run (or disarm a workflow's start nodes). With `before_step`, \
         set a BREAKPOINT instead: the run keeps going and pauses just before \
         that step starts, so it can be inspected in the state it is in rather \
         than one effect later. Durable — it survives a restart.",
        obj(
            json!({"run": s("Run id"), "name": s("Workflow name"),
                   "before_step": s("Pause just before this step starts (a breakpoint)")}),
            &[],
        ),
        open_obj(
            json!({"ok": {"type": "boolean"}, "break_before": {"type": "string"}}),
            &[],
        ),
        true,
        ROOT_ONLY,
    );
    c(
        "workflow.resume",
        "workflow",
        "Resume a paused run (or re-arm a workflow).",
        obj(json!({"run": s("Run id"), "name": s("Workflow name")}), &[]),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        ROOT_ONLY,
    );
    c(
        "workflow.signal",
        "workflow",
        "Send a named signal (with a payload) into a run, or start a workflow whose start node listens for it.",
        obj(
            json!({"name": s("Signal name"), "payload": any(), "run": s("Target run id (optional)")}),
            &["name"],
        ),
        open_obj(json!({"delivered": {"type": "integer"}}), &["delivered"]),
        true,
        ALL,
    );
    c(
        "workflow.wait",
        "workflow",
        "Wait for a run to finish and return its output.",
        obj(
            json!({"run": s("Run id"), "timeout": s("Duration")}),
            &["run"],
        ),
        open_obj(
            json!({"run": {"type": "string"}, "status": {"type": "string"}, "output": any()}),
            &["run", "status"],
        ),
        true,
        ROOT_WF,
    );

    // ---- plan ----
    c(
        "plan.create",
        "plan",
        "Create (or replace) this conversation's working plan: a goal and an ordered list of items.",
        obj(
            json!({"goal": s("The goal"), "items": arr(json!({"oneOf": [{"type": "string"}, open_obj(json!({"title": {"type": "string"}, "detail": {"type": "string"}}), &["title"])]}))}),
            &["goal", "items"],
        ),
        open_obj(
            json!({"goal": {"type": "string"}, "items": arr(json!({"type": "object"}))}),
            &["goal", "items"],
        ),
        true,
        ALL,
    );
    c(
        "plan.get",
        "plan",
        "Read this conversation's plan.",
        obj(json!({}), &[]),
        open_obj(json!({"plan": any(), "progress": {"type": "string"}}), &[]),
        true,
        ALL,
    );
    c(
        "plan.update",
        "plan",
        "Advance the plan: set an item's status/note, bind it to a run/subagent, insert an item, or reorder.",
        obj(
            json!({
                "item": {"description": "Item id (number) or exact title", "oneOf": [{"type": "integer"}, {"type": "string"}]},
                "status": {"enum": ["pending", "in_progress", "done", "blocked", "skipped"]},
                "note": s("A short note"), "title": s("New title"), "detail": s("New detail"),
                "bind": open_obj(json!({"run": {"type": "string"}, "subagent": {"type": "string"}, "task": {"type": "string"}}), &[]),
                "insert": open_obj(json!({"title": {"type": "string"}, "detail": {"type": "string"}, "after": {"type": "integer"}}), &["title"]),
                "reorder": arr(json!({"type": "integer"}))
            }),
            &[],
        ),
        open_obj(
            json!({"goal": {"type": "string"}, "items": arr(json!({"type": "object"})), "progress": {"type": "string"}}),
            &["goal", "items"],
        ),
        true,
        ALL,
    );
    c(
        "plan.clear",
        "plan",
        "Clear the plan (the goal is met or abandoned).",
        obj(json!({}), &[]),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        ALL,
    );

    // ---- misc ----
    c(
        "ask_human",
        "human",
        "Ask the human (the conversation's principal) a question and wait for the answer.",
        obj(
            json!({"question": s("The question"), "schema": {"type": "object", "description": "Expected answer shape"}, "to": s("Principal or conversation to ask (default: the current one)"), "timeout": s("Duration")}),
            &["question"],
        ),
        open_obj(
            json!({"reply": any(), "timed_out": {"type": "boolean"}}),
            &[],
        ),
        true,
        ALL,
    );
    c(
        "sleep",
        "time",
        "Wait for a duration (durable: survives restarts).",
        obj(
            json!({"duration": s("Duration, e.g. 30s, 5m")}),
            &["duration"],
        ),
        open_obj(json!({"slept_ms": {"type": "integer"}}), &["slept_ms"]),
        true,
        ALL,
    );
    c(
        "await",
        "time",
        "Wait until a condition holds (CEL over memory/resources/steps/signals) or a timeout elapses.",
        obj(
            json!({"condition": s("CEL expression"), "on": arr(json!({"type": "string"})), "timeout": s("Duration")}),
            &["condition"],
        ),
        open_obj(
            json!({"satisfied": {"type": "boolean"}, "value": any()}),
            &["satisfied"],
        ),
        true,
        ALL,
    );
    c(
        "context.compact",
        "context",
        "Compact this context: summarize older messages, keep the recent ones verbatim.",
        obj(
            json!({"target_tokens": {"type": "integer"}, "keep_last": {"type": "integer"}}),
            &[],
        ),
        open_obj(
            json!({"version": {"type": "integer"}, "est_tokens": {"type": "integer"}, "folded": {"type": "integer"}}),
            &["version"],
        ),
        true,
        ALL,
    );
    c(
        "think",
        "intelligence",
        "One structured reasoning call (no tools): give a prompt and optionally an output schema; get the object back.",
        obj(
            json!({"prompt": s("What to think about"), "output_schema": {"type": "object"}, "reads": arr(json!({"type": "string"})), "skills": arr(json!({"type": "string"}))}),
            &["prompt"],
        ),
        any(),
        true,
        ALL,
    );
    c(
        "finish",
        "lifecycle",
        "Finish the current unit of work with a status and an optional output.",
        obj(
            json!({"status": {"enum": ["completed", "failed", "refused", "cancelled"]}, "output": any(), "reason": s("Why"), "exit": {"type": "boolean", "description": "Root only: exit the daemon"}}),
            &["status"],
        ),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        DefaultGrant {
            root: true,
            workflows: false,
            subagents: true,
            user: false,
            agent: false,
        },
    );
    c(
        "status",
        "status",
        "The instance status: runs, subagents, conversations, budget, store.",
        obj(json!({}), &[]),
        json!({"type": "object"}),
        true,
        DefaultGrant {
            root: true,
            workflows: true,
            subagents: true,
            user: true,
            agent: true,
        },
    );

    // ---- knowledge / search (profiles, mapping-only) ----
    c(
        "knowledge.search",
        "knowledge",
        "Search the knowledge base (RAG over documents).",
        obj(
            json!({"query": s("The query"), "top_k": {"type": "integer", "minimum": 1}, "filters": {"type": "object"}}),
            &["query"],
        ),
        open_obj(
            json!({"hits": arr(open_obj(json!({"id": {"type": "string"}, "uri": {"type": "string"}, "title": {"type": "string"}, "score": {"type": "number"}, "snippet": {"type": "string"}, "metadata": {"type": "object"}}), &[]))}),
            &["hits"],
        ),
        false,
        ALL,
    );
    c(
        "knowledge.get",
        "knowledge",
        "Fetch a knowledge document by id or URI.",
        obj(
            json!({"id": s("Document id"), "uri": s("Document URI")}),
            &[],
        ),
        open_obj(
            json!({"content": {"type": "string"}, "mime": {"type": "string"}, "metadata": {"type": "object"}}),
            &["content"],
        ),
        false,
        ALL,
    );
    c(
        "knowledge.list",
        "knowledge",
        "List knowledge documents.",
        obj(json!({"prefix": s("Prefix")}), &[]),
        open_obj(json!({"docs": arr(json!({"type": "object"}))}), &["docs"]),
        false,
        ALL,
    );
    c(
        "search.query",
        "search",
        "Web/docs/code search through the search server.",
        obj(
            json!({"query": s("The query"), "kind": {"enum": ["web", "docs", "code"]}, "limit": {"type": "integer", "minimum": 1}, "freshness": s("e.g. day, week")}),
            &["query"],
        ),
        open_obj(
            json!({"results": arr(open_obj(json!({"title": {"type": "string"}, "url": {"type": "string"}, "snippet": {"type": "string"}, "source": {"type": "string"}, "published": {"type": "string"}}), &[]))}),
            &["results"],
        ),
        false,
        ALL,
    );
    c(
        "search.fetch",
        "search",
        "Fetch a page's content through the search server.",
        obj(
            json!({"url": s("The URL"), "max_bytes": {"type": "integer"}}),
            &["url"],
        ),
        open_obj(
            json!({"content": {"type": "string"}, "mime": {"type": "string"}, "final_url": {"type": "string"}}),
            &["content"],
        ),
        false,
        ALL,
    );

    // ---- skills ----
    c(
        "skills.list",
        "skills",
        "List the available skills (name, description, when to use).",
        obj(json!({}), &[]),
        open_obj(
            json!({"skills": arr(json!({"type": "object"}))}),
            &["skills"],
        ),
        true,
        ALL,
    );
    c(
        "skills.load",
        "skills",
        "Load a skill's full instructions into this context.",
        obj(
            json!({"name": s("Skill name"), "version": s("Version/hash (optional)"), "arguments": {"type": "object"}}),
            &["name"],
        ),
        open_obj(
            json!({"loaded": {"type": "boolean"}, "name": {"type": "string"}, "hash": {"type": "string"}, "body": {"type": "string"}}),
            &["loaded"],
        ),
        true,
        ALL,
    );
    c(
        "skills.unload",
        "skills",
        "Drop a loaded skill from this context.",
        obj(json!({"name": s("Skill name")}), &["name"]),
        open_obj(json!({"ok": {"type": "boolean"}}), &["ok"]),
        true,
        ALL,
    );

    // ---- exec (guarded local command runner; DEFAULT-OFF) -------------------
    // A mapping-only contract by default: agentd runs no local code unless an
    // operator both builds `--features exec` AND sets `security.exec`. Two
    // independent switches, because arbitrary local execution is the one
    // capability that turns a prompt-injection into host compromise. Failing
    // either, `exec` is delegated off-box via `tools.overrides`. It always
    // carries the `sensitive` + `egress` trifecta tags (attached in
    // `Registry::build`), so the Rule-of-Two gate refuses to combine it with
    // untrusted input.
    c(
        "exec",
        "exec",
        "Run a local command (argv — NO shell interpretation) and return {stdout, stderr, exit_code, timed_out}. GUARDED and default-OFF: runs only allow-listed commands, confined to a working directory, with a timeout, an output cap, and a minimal environment. Enable a local runner via `security.exec` in a build with `--features exec`, or map it onto an MCP server with `tools.overrides` to delegate execution off-box.",
        obj(
            json!({
                "cmd": s("The command to run (argv[0]) — must be in security.exec.allow"),
                "args": arr(s("Arguments (argv[1..]); passed directly, never through a shell")),
                "cwd": s("Working directory, relative to and confined within security.exec.workdir"),
                "stdin": s("Optional standard input for the command"),
                "timeout": s("Max wall-clock (e.g. `10s`); clamped to the configured maximum")
            }),
            &["cmd"],
        ),
        open_obj(
            json!({
                "stdout": {"type": "string"}, "stderr": {"type": "string"},
                "exit_code": {"type": "integer"}, "timed_out": {"type": "boolean"}
            }),
            &["stdout", "stderr", "exit_code"],
        ),
        false,
        ALL,
    );
    v
}

/// The contract names, in table order.
pub fn names() -> Vec<&'static str> {
    contracts().into_iter().map(|c| c.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_are_unique_well_formed_and_cover_the_catalogue() {
        let all = contracts();
        let mut seen = std::collections::BTreeSet::new();
        for c in &all {
            assert!(seen.insert(c.name), "duplicate contract {}", c.name);
            crate::jsonschema::check_schema(&c.input)
                .unwrap_or_else(|e| panic!("{}: bad input schema: {e:?}", c.name));
            crate::jsonschema::check_schema(&c.output)
                .unwrap_or_else(|e| panic!("{}: bad output schema: {e:?}", c.name));
            assert!(
                c.name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'),
                "{}",
                c.name
            );
        }
        for must in [
            "instruction.read",
            "instruction.subscribe",
            "subagent.run",
            "subagent.send",
            "subagent.kill",
            "subagent.status",
            "subagent.await",
            "subagent.list",
            "subagent.retire",
            "code.run",
            "memory.get",
            "memory.set",
            "memory.list",
            "memory.push",
            "memory.shift",
            "memory.pop",
            "memory.delete",
            "artifact.create",
            "artifact.get",
            "artifact.delete",
            "artifact.list",
            "workflow.run",
            "workflow.create",
            "workflow.update",
            "workflow.delete",
            "workflow.list",
            "workflow.status",
            "workflow.cancel",
            "workflow.pause",
            "workflow.resume",
            "workflow.signal",
            "workflow.wait",
            "plan.create",
            "plan.get",
            "plan.update",
            "plan.clear",
            "ask_human",
            "sleep",
            "await",
            "context.compact",
            "think",
            "finish",
            "status",
            "knowledge.search",
            "knowledge.get",
            "knowledge.list",
            "search.query",
            "search.fetch",
            "skills.list",
            "skills.load",
            "skills.unload",
        ] {
            assert!(seen.contains(must), "missing contract {must}");
        }
        // Mapping-only contracts have no built-in. (`exec` is mapping-only in the
        // catalogue; a local runner is turned on in `Registry::build` under the
        // `exec` feature + `security.exec`.)
        for c in &all {
            let mapping_only = c.name == "code.run"
                || c.name == "exec"
                || c.name.starts_with("knowledge.")
                || c.name.starts_with("search.");
            assert_eq!(!c.builtin, mapping_only, "{}", c.name);
        }
        // finish is not granted to workflows (they use the finish step).
        assert!(
            !all.iter()
                .find(|c| c.name == "finish")
                .unwrap()
                .grant
                .workflows
        );
        assert!(all.iter().find(|c| c.name == "status").unwrap().grant.user);
    }
}
