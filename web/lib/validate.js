// Graph validation for the editor — a faithful port of the checks the runtime
// runs in `parse_workflow` / `validate_graph` (crates/agentd/src/engine/model.rs).
//
// The point of porting rather than approximating: a graph the editor calls
// valid must be a graph `agentd --validate-config` also calls valid. Every rule
// below cites the Rust message it mirrors, so the two can be diffed by eye when
// the dialect moves.
import { NODES, nodeInfo, isStart } from "./nodeRegistry";

/** `[a-zA-Z_][a-zA-Z0-9_-]{0,63}` — model.rs `valid_id`. */
export function validId(s) {
  return /^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$/.test(s || "");
}

/** Cross-cutting fields every step may carry, whatever its kind. */
const COMMON = new Set([
  "kind",
  "depends_on",
  "when",
  "if",
  "on_error",
  "retry",
  "timeout",
  "foreach",
  "concurrency",
  "description",
  "label",
  "on_replay",
  "output_schema",
  "writes",
  "mode",
]);

/**
 * Can `source` feed `target`?
 *
 * Two rules, both from the runtime:
 *   * a start node has no inbound edge  ("a start node cannot depend on other steps")
 *   * a step cannot depend on itself     ("depends on itself")
 * Everything else is allowed — the dialect's dependency edge is untyped, so
 * inventing extra restrictions here would make the editor refuse graphs the
 * runtime accepts.
 */
export function canConnect(sourceKind, targetKind, sourceId, targetId) {
  if (sourceId && targetId && sourceId === targetId) {
    return { ok: false, why: "a step cannot depend on itself" };
  }
  if (isStart(targetKind)) {
    return {
      ok: false,
      why: `${targetKind} is a start node — it fires the run, so nothing can precede it`,
    };
  }
  if (sourceKind && ["finish", "fail"].includes(sourceKind)) {
    return { ok: false, why: `${sourceKind} ends the run — nothing runs after it` };
  }
  return { ok: true };
}

/** Kahn's algorithm; returns the ids it could order. */
function topoOrder(nodes, edges) {
  const indeg = new Map(nodes.map((n) => [n.id, 0]));
  const out = new Map(nodes.map((n) => [n.id, []]));
  for (const e of edges) {
    if (!indeg.has(e.target) || !out.has(e.source)) continue;
    indeg.set(e.target, indeg.get(e.target) + 1);
    out.get(e.source).push(e.target);
  }
  const queue = [...indeg.entries()].filter(([, d]) => d === 0).map(([id]) => id);
  const order = [];
  while (queue.length) {
    const id = queue.shift();
    order.push(id);
    for (const t of out.get(id) || []) {
      indeg.set(t, indeg.get(t) - 1);
      if (indeg.get(t) === 0) queue.push(t);
    }
  }
  return order;
}

/**
 * Validate one workflow model ({name, nodes, edges}).
 * Returns `{errors: [{id, message}], warnings: [...]}` — `id` is the step the
 * problem belongs to, so the canvas can mark it.
 */
export function validateWorkflow(wf) {
  const errors = [];
  const warnings = [];
  const err = (message, id = null) => errors.push({ id, message });
  const warn = (message, id = null) => warnings.push({ id, message });

  const nodes = wf.nodes || [];
  const edges = wf.edges || [];

  if (!validId(wf.name)) {
    err(`workflow name "${wf.name || ""}" must match [a-zA-Z_][a-zA-Z0-9_-]{0,63}`);
  }
  if (!nodes.length) {
    err("a workflow needs at least one step");
    return { errors, warnings };
  }

  // depends_on, derived from the edges
  const deps = new Map(nodes.map((n) => [n.id, []]));
  for (const e of edges) {
    if (deps.has(e.target)) deps.get(e.target).push(e.source);
  }

  const ids = new Set(nodes.map((n) => n.id));
  const starts = nodes.filter((n) => isStart(n.data.kind));

  for (const n of nodes) {
    const kind = n.data.kind;
    const info = nodeInfo(kind);
    const at = `step "${n.id}"`;

    if (!validId(n.id)) err(`${at}: id must match [a-zA-Z_][a-zA-Z0-9_-]{0,63}`, n.id);
    if (!NODES[kind]) err(`${at}: unknown kind "${kind}"`, n.id);
    else if (info.implemented === false) {
      err(`${at}: kind "${kind}" is not available in this build yet`, n.id);
    }

    // Strict per-kind fields — the runtime refuses unknown ones outright.
    const allowed = new Set([...(info.fields || []), ...COMMON]);
    for (const [k, v] of Object.entries(n.data.spec || {})) {
      if (!allowed.has(k)) {
        err(
          `${at}: unknown field "${k}" for kind "${kind}" (allowed: ${(info.fields || []).join(", ") || "none"})`,
          n.id,
        );
      } else if (v === "" || v === undefined || v === null) {
        if ((info.required || []).includes(k)) {
          err(`${at}: kind "${kind}" requires field "${k}"`, n.id);
        }
      }
    }
    for (const req of info.required || []) {
      const val = (n.data.spec || {})[req];
      if (val === undefined || val === null || val === "") {
        if (!Object.prototype.hasOwnProperty.call(n.data.spec || {}, req)) {
          err(`${at}: kind "${kind}" requires field "${req}"`, n.id);
        }
      }
    }

    // Edges
    const myDeps = deps.get(n.id) || [];
    if (isStart(kind) && myDeps.length) {
      err(`${at}: a start node cannot depend on other steps`, n.id);
    }
    if (!isStart(kind) && myDeps.length === 0) {
      err(`${at}: a non-start step must depend on something (unreachable root)`, n.id);
    }
    for (const d of myDeps) {
      if (!ids.has(d)) err(`${at}: depends_on names unknown step "${d}"`, n.id);
      if (d === n.id) err(`${at}: depends on itself`, n.id);
    }
  }

  if (!starts.length) {
    err("a workflow needs a start node (once, schedule, loop, subscribe, signal, event, a2a, manual)");
  }

  // Acyclic
  const order = topoOrder(nodes, edges);
  if (order.length !== nodes.length) {
    const stuck = nodes.filter((n) => !order.includes(n.id)).map((n) => n.id);
    err(`cycle among steps: ${stuck.join(", ")}`);
    stuck.forEach((id) => err(`step "${id}": part of a cycle`, id));
  }

  // Reachability from a start node
  const reachable = new Set(starts.map((s) => s.id));
  let changed = true;
  while (changed) {
    changed = false;
    for (const n of nodes) {
      if (reachable.has(n.id)) continue;
      const myDeps = deps.get(n.id) || [];
      if (myDeps.length && myDeps.some((d) => reachable.has(d))) {
        reachable.add(n.id);
        changed = true;
      }
    }
  }
  for (const n of nodes) {
    if (!reachable.has(n.id)) err(`step "${n.id}": not reachable from any start node`, n.id);
  }

  // A terminal step is mandatory.
  if (!nodes.some((n) => n.data.kind === "finish")) {
    err("a `finish` step is required");
  }

  // Advisory — legal, but usually a mistake.
  if (starts.length > 1) {
    warn(`${starts.length} start nodes: every one of them fires its own run`);
  }
  for (const n of nodes) {
    if (["finish", "fail"].includes(n.data.kind)) {
      const outgoing = edges.filter((e) => e.source === n.id);
      outgoing.forEach(() => warn(`step "${n.id}" ends the run — steps after it never execute`, n.id));
    }
  }

  return { errors, warnings };
}

/** Validate every workflow in the document. */
export function validateAll(workflows) {
  const seen = new Set();
  const out = workflows.map((wf) => {
    const r = validateWorkflow(wf);
    if (seen.has(wf.name)) r.errors.push({ id: null, message: `duplicate workflow name "${wf.name}"` });
    seen.add(wf.name);
    return { name: wf.name, ...r };
  });
  return out;
}
