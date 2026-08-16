// The workflow node registry that drives the editor palette and property forms.
// The data (`workflow-nodes.json`) is generated from the agentd binary's
// `--workflow-schema` output — the single source of truth for the node catalogue
// (RFC 0027 dialect 3). Regenerate with:
//   agentd --workflow-schema | jq … > web/lib/workflow-nodes.json   (see docs)
import NODES from "./workflow-nodes.json";

export { NODES };

// Ordered categories for the palette. Each carries a display label and an accent
// color (drawn from the site's dark-terminal palette) used for the node header.
export const CATEGORIES = [
  { id: "trigger", label: "Triggers", accent: "#4ade80", hint: "start a run" },
  { id: "intelligence", label: "Intelligence", accent: "#38bdf8", hint: "model turns" },
  { id: "control", label: "Control flow", accent: "#f59e0b", hint: "branch / loop / fan-out" },
  { id: "data", label: "Data", accent: "#a78bfa", hint: "transform values" },
  { id: "io", label: "I/O & integration", accent: "#2dd4bf", hint: "http, mcp, memory" },
  { id: "orchestration", label: "Orchestration", accent: "#fb7185", hint: "wait, join, child runs" },
  { id: "terminal", label: "Terminal", accent: "#c084fc", hint: "end the run" },
];

const ACCENT = Object.fromEntries(CATEGORIES.map((c) => [c.id, c.accent]));

/** The registry entry for a kind, or a permissive fallback for unknown kinds. */
export function nodeInfo(kind) {
  return (
    NODES[kind] || {
      kind,
      start: false,
      category: "other",
      fields: [],
      required: [],
      implemented: true,
    }
  );
}

/** The accent color for a kind's category. */
export function accentFor(kind) {
  return ACCENT[nodeInfo(kind).category] || "#8b8b94";
}

/** Is this a start node (a trigger that produces runs)? */
export function isStart(kind) {
  return !!nodeInfo(kind).start;
}

/** All kinds in a category, sorted, for the palette. */
export function kindsInCategory(catId) {
  return Object.values(NODES)
    .filter((n) => n.category === catId)
    .map((n) => n.kind)
    .sort((a, b) => a.localeCompare(b));
}

/** A short, human description for a handful of the most-used kinds (tooltip). */
export const BLURBS = {
  once: "fire a single run at startup",
  manual: "fire on explicit invocation (A2A / operator)",
  schedule: "fire on a clock (cron / every / at)",
  loop: "fire repeatedly, self-paced",
  subscribe: "fire on an MCP resource notification",
  signal: "fire on an in-process signal",
  event: "fire on a lifecycle event",
  webhook: "fire on an inbound HTTP request (dedicated listener)",
  agent: "take an agent turn (ReAct over granted tools)",
  think: "a preset intelligence call",
  classify: "label an input into one of `classes`",
  extract: "pull structured data against a schema",
  summarize: "condense text",
  judge: "score / verify a claim",
  http: "outbound REST call — also emits webhooks (sign)",
  "mcp.tool": "call an MCP tool",
  wait: "suspend until a signal / condition / callback",
  join: "barrier over multiple runs or branches",
  workflow: "run a child workflow (sync / async / detached)",
  subagent: "spawn a scoped subagent",
  foreach: "fan out over an array (durable batches)",
  parallel: "run named branches concurrently, fan-in",
  race: "first branch to finish wins; cancel the rest",
  switch: "route to one dependent by value",
  assign: "compute and store a blackboard value",
  finish: "end the run successfully",
  fail: "end the run as failed",
};

/**
 * A minimal starter spec for a freshly-dropped node: seed the required fields
 * (and a couple of common optional ones) with empty placeholders so the property
 * panel has something to edit.
 */
export function starterSpec(kind) {
  const info = nodeInfo(kind);
  const spec = {};
  const seed = new Set([...info.required]);
  // A few friendly defaults so a new node is immediately meaningful.
  const DEFAULTS = {
    http: { method: "GET" },
    agent: { instruction: "" },
    webhook: { path: "/hooks/", methods: ["POST"] },
    finish: { status: "completed" },
    schedule: { every: "5m" },
    loop: {},
    wait: { on: "signal" },
    workflow: { mode: "sync" },
  };
  for (const f of seed) spec[f] = "";
  Object.assign(spec, DEFAULTS[kind] || {});
  return spec;
}
