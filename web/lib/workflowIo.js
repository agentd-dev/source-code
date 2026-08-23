// Convert between an agentd `config_version: "1"` document (YAML) and the
// editor's graph model. A workflow's `steps` map becomes React Flow nodes; every
// `depends_on` entry becomes an edge (source → target means "target depends on
// source"). The rest of the config document is preserved verbatim so an
// import → edit → export round-trips the whole file, not just the graph.
import yaml from "js-yaml";

// Fields the editor manages structurally on the node itself (not in the free-form
// spec): `kind` becomes the node type, `depends_on` becomes edges.
const STRUCTURAL = new Set(["kind", "depends_on"]);

let _seq = 0;
const uid = (p) => `${p}_${(_seq++).toString(36)}${Date.now().toString(36).slice(-3)}`;

/** Parse a YAML config document into { doc, workflows: [editorWorkflow] }. */
export function parseConfig(text) {
  const doc = yaml.load(text) || {};
  if (typeof doc !== "object" || Array.isArray(doc)) {
    throw new Error("config must be a YAML mapping (config_version: \"1\")");
  }
  const wfs = Array.isArray(doc.workflows) ? doc.workflows : [];
  const workflows = wfs.map((wf, i) => workflowToModel(wf, i));
  // Strip `workflows` from the retained doc; it is rebuilt on export.
  const { workflows: _omit, ...rest } = doc;
  return { doc: rest, workflows: workflows.length ? workflows : [emptyWorkflow()] };
}

/** One workflow object → { id, name, meta, nodes, edges }. */
export function workflowToModel(wf, index = 0) {
  const steps = wf && typeof wf.steps === "object" && wf.steps ? wf.steps : {};
  const { steps: _s, name, ...meta } = wf || {};
  const nodes = [];
  const edges = [];
  for (const [stepId, raw] of Object.entries(steps)) {
    const step = raw && typeof raw === "object" ? raw : { kind: String(raw) };
    const spec = {};
    for (const [k, v] of Object.entries(step)) {
      if (!STRUCTURAL.has(k)) spec[k] = v;
    }
    nodes.push({
      id: stepId,
      type: "wf",
      position: { x: 0, y: 0 }, // laid out below
      data: { kind: step.kind || "noop", spec },
    });
    const deps = Array.isArray(step.depends_on) ? step.depends_on : [];
    for (const dep of deps) {
      edges.push({ id: uid("e"), source: String(dep), target: stepId });
    }
  }
  layout(nodes, edges);
  return {
    id: uid("wf"),
    name: name || `workflow_${index + 1}`,
    meta, // version, description, concurrency, limits, inputs, outputs, armed …
    nodes,
    edges,
  };
}

/** Rebuild the full config document (as YAML text) from the editor model. */
export function serializeConfig(doc, workflows) {
  const out = { ...doc };
  out.workflows = workflows.map((wf) => modelToWorkflow(wf));
  // Keep a stable, readable key order: config_version first if present.
  const ordered = orderKeys(out, [
    "config_version",
    "agent",
    "intelligence",
    "mcp",
    "store",
    "webhooks",
    "goal",
    "workflows",
    "lifecycle",
    "observability",
  ]);
  return yaml.dump(ordered, { lineWidth: -1, noRefs: true, sortKeys: false });
}

/** One editor workflow → a plain workflow object for the config. */
export function modelToWorkflow(wf) {
  const depsByTarget = new Map();
  for (const e of wf.edges) {
    if (!depsByTarget.has(e.target)) depsByTarget.set(e.target, []);
    depsByTarget.get(e.target).push(e.source);
  }
  const steps = {};
  for (const n of wf.nodes) {
    const step = { kind: n.data.kind };
    const deps = depsByTarget.get(n.id);
    if (deps && deps.length) step.depends_on = deps;
    // Emit spec fields, skipping empty strings/nulls the user never filled.
    for (const [k, v] of Object.entries(n.data.spec || {})) {
      if (v === "" || v === null || v === undefined) continue;
      step[k] = v;
    }
    steps[n.id] = step;
  }
  const wfObj = { name: wf.name, ...wf.meta, steps };
  return wfObj;
}

export function emptyWorkflow(name) {
  const id = uid("wf");
  return {
    id,
    name: name || "new_workflow",
    meta: {},
    nodes: [
      { id: "start", type: "wf", position: { x: 40, y: 120 }, data: { kind: "once", spec: {} } },
      {
        id: "done",
        type: "wf",
        position: { x: 360, y: 120 },
        data: { kind: "finish", spec: { status: "completed" } },
      },
    ],
    edges: [{ id: uid("e"), source: "start", target: "done" }],
  };
}

// --- layered layout ---------------------------------------------------------
// A deterministic longest-path layering: rank = longest chain from any root,
// then stack nodes within a rank. Good enough for readable auto-layout on import;
// the user can drag freely afterwards.
export function layout(nodes, edges, opts = {}) {
  const dx = opts.dx ?? 240;
  const dy = opts.dy ?? 110;
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const preds = new Map(nodes.map((n) => [n.id, []]));
  const succs = new Map(nodes.map((n) => [n.id, []]));
  for (const e of edges) {
    if (byId.has(e.source) && byId.has(e.target)) {
      succs.get(e.source).push(e.target);
      preds.get(e.target).push(e.source);
    }
  }
  const rank = new Map();
  const visiting = new Set();
  const rankOf = (id) => {
    if (rank.has(id)) return rank.get(id);
    if (visiting.has(id)) return 0; // cycle guard (dialect-3 is a DAG, but be safe)
    visiting.add(id);
    const p = preds.get(id) || [];
    const r = p.length ? Math.max(...p.map((x) => rankOf(x) + 1)) : 0;
    visiting.delete(id);
    rank.set(id, r);
    return r;
  };
  for (const n of nodes) rankOf(n.id);
  const perRank = new Map();
  for (const n of nodes) {
    const r = rank.get(n.id) || 0;
    const row = perRank.get(r) || 0;
    n.position = { x: 40 + r * dx, y: 60 + row * dy };
    perRank.set(r, row + 1);
  }
  return nodes;
}

// Order the top-level keys of an object for a readable dump; unknown keys keep
// their insertion order after the known ones.
function orderKeys(obj, order) {
  const out = {};
  for (const k of order) if (k in obj) out[k] = obj[k];
  for (const k of Object.keys(obj)) if (!(k in out)) out[k] = obj[k];
  return out;
}

export function newStepId(existing, kind) {
  const base = kind.replace(/[^a-zA-Z0-9]/g, "_").replace(/^_+|_+$/g, "") || "step";
  let i = 1;
  let id = base;
  const taken = new Set(existing);
  while (taken.has(id)) id = `${base}_${i++}`;
  return id;
}
