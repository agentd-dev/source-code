// @agentd/sdk — author agentd workflows in TypeScript, compile to the
// signed TOML the runtime executes. TOML stays the compile target; this
// is the authoring surface. Zero runtime dependencies.

/** @typedef {string | number | boolean | string[]} Scalar */

function escape(s) {
  return s
    .replace(/\\/g, "\\\\")
    .replace(/"/g, '\\"')
    .replace(/\n/g, "\\n")
    .replace(/\t/g, "\\t");
}

function emitScalar(v) {
  if (Array.isArray(v)) return `[${v.map((x) => `"${escape(String(x))}"`).join(", ")}]`;
  if (typeof v === "string") return `"${escape(v)}"`;
  if (typeof v === "boolean") return v ? "true" : "false";
  return String(v); // number
}

// Emit `key = value` lines for a flat object, skipping undefined.
function emitFields(obj, order = []) {
  const keys = [...order, ...Object.keys(obj).filter((k) => !order.includes(k))];
  let out = "";
  for (const k of keys) {
    const v = obj[k];
    if (v === undefined || v === null) continue;
    out += `${k} = ${emitScalar(v)}\n`;
  }
  return out;
}

/** Node-kind factories. Each returns a `{ type, ... }` spec. */
export const node = {
  llmInfer: (o) => ({ type: "llm_infer", backend: o.backend, prompt: o.prompt, input_from: o.inputFrom, output_schema: o.outputSchema }),
  agentLoop: (o) => ({ type: "agent_loop", backend: o.backend, instructions: o.instructions, instructions_from: o.instructionsFrom, tools: o.tools, max_steps: o.maxSteps, max_tokens: o.maxTokens }),
  switch: (o) => ({ type: "switch", expr: o.expr }),
  condition: (o) => ({ type: "condition", expr: o.expr }),
  merge: () => ({ type: "merge" }),
  terminate: () => ({ type: "terminate" }),
  fail: (o) => ({ type: "fail", reason: o?.reason }),
  pauseForApproval: (o) => ({ type: "pause_for_approval", reason: o?.reason }),
  readEnv: (o) => ({ type: "read_env", key: o.key }),
  readFile: (o) => ({ type: "read_file", path_from: o.pathFrom }),
  writeFile: (o) => ({ type: "write_file", path_from: o.pathFrom, content_from: o.contentFrom }),
  createDir: (o) => ({ type: "create_dir", path_from: o.pathFrom }),
  parseJson: (o) => ({ type: "parse_json", input_from: o.inputFrom }),
  jsonSelect: (o) => ({ type: "json_select", input_from: o.inputFrom, path: o.path }),
  templateRender: (o) => ({ type: "template_render", template: o.template, input_from: o.inputFrom }),
  httpRequest: (o) => ({ type: "http_request", method: o.method, url_from: o.urlFrom, body_from: o.bodyFrom }),
  call: (o) => ({ type: "call", workflow: o.workflow, input_from: o.inputFrom, start: o.start }),
  respond: (o) => ({ type: "respond", status: o?.status, content_type: o?.contentType, body_template: o.bodyTemplate, input_from: o?.inputFrom }),
  map: (o) => ({ type: "map", items_from: o.itemsFrom, workflow: o.workflow, start: o.start, max_items: o.maxItems, max_concurrent: o.maxConcurrent }),
};

export class Workflow {
  constructor(name) {
    this.name = name;
    this._starts = [];
    this._nodes = [];
    this._edges = [];
    this._policy = undefined;
  }

  /** Declare a start node entering at `entryNode`. */
  start(name, entryNode, source = "manual") {
    this._starts.push({ name, source, entry_node: entryNode });
    return this;
  }

  /** Add a node with a unique id. */
  node(id, spec) {
    if (this._nodes.some((n) => n.id === id)) throw new Error(`duplicate node id "${id}"`);
    this._nodes.push({ id, spec });
    return this;
  }

  /** Add an edge, optionally a `when`-labelled branch. */
  edge(from, to, opts) {
    this._edges.push({ from, to, when: opts?.when });
    return this;
  }

  /** Attach a policy (the allowlist the runtime enforces). */
  policy(p) {
    this._policy = p;
    return this;
  }

  /** Compile to workflow TOML. */
  toToml() {
    if (this._starts.length === 0) throw new Error("workflow has no start nodes");
    if (this._nodes.length === 0) throw new Error("workflow has no nodes");

    let out = `name = ${emitScalar(this.name)}\n`;

    if (this._policy) {
      for (const [family, table] of Object.entries(this._policy)) {
        const fields = emitFields(table);
        if (fields.trim()) out += `\n[policy.${family}]\n${fields}`;
      }
    }

    for (const s of this._starts) {
      out += `\n[[start_nodes]]\n${emitFields(s, ["name", "source", "entry_node"])}`;
    }
    for (const { id, spec } of this._nodes) {
      out += `\n[[nodes]]\n${emitFields({ id, ...spec }, ["id", "type"])}`;
    }
    for (const e of this._edges) {
      out += `\n[[edges]]\n${emitFields(e, ["from", "to", "when"])}`;
    }
    return out;
  }
}

/** Start a new workflow. */
export const workflow = (name) => new Workflow(name);
