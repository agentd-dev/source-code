"use client";

// A visual editor for agentd workflows (RFC 0027 dialect 3). Nodes are steps;
// edges are `depends_on`. The palette and property forms are driven by the node
// registry generated from `agentd --workflow-schema`. Import a YAML config to
// edit it, export the whole document back out. Multi-workflow: a config can hold
// several workflows, switched by the tabs.
import { useCallback, useMemo, useRef, useState } from "react";
import {
  ReactFlow,
  ReactFlowProvider,
  Background,
  Controls,
  MiniMap,
  Handle,
  Position,
  applyNodeChanges,
  applyEdgeChanges,
  addEdge,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import {
  CATEGORIES,
  accentFor,
  isStart,
  nodeInfo,
  kindsInCategory,
  BLURBS,
  NODES,
  starterSpec,
} from "../../lib/nodeRegistry";
import { parseConfig, serializeConfig, emptyWorkflow, layout, newStepId } from "../../lib/workflowIo";

const DEFAULT_DOC = { config_version: "2" };

// ── custom node ─────────────────────────────────────────────────────────────
function WfNode({ id, data, selected }) {
  const info = nodeInfo(data.kind);
  const accent = accentFor(data.kind);
  const start = isStart(data.kind);
  return (
    <div
      style={{
        borderColor: selected ? accent : "var(--line)",
        boxShadow: selected ? `0 0 0 1px ${accent}` : "none",
      }}
      className="min-w-[132px] rounded-md border bg-[var(--panel)] text-[13px]"
      title={BLURBS[data.kind] || data.kind}
    >
      {!start && (
        <Handle type="target" position={Position.Left} style={{ background: accent }} />
      )}
      <div
        className="flex items-center gap-1.5 rounded-t-md px-2 py-1"
        style={{ background: `${accent}1a`, borderBottom: `1px solid ${accent}55` }}
      >
        <span className="h-2 w-2 shrink-0 rounded-full" style={{ background: accent }} />
        <span className="font-mono font-semibold" style={{ color: accent }}>
          {data.kind}
        </span>
        {!info.implemented && (
          <span className="ml-auto rounded bg-[var(--line)] px-1 text-[9px] text-[var(--dim)]">soon</span>
        )}
      </div>
      <div className="truncate px-2 py-1 font-mono text-[var(--fg)]">{id}</div>
      <Handle type="source" position={Position.Right} style={{ background: accent }} />
    </div>
  );
}
const NODE_TYPES = { wf: WfNode };

// ── main editor ─────────────────────────────────────────────────────────────
function Editor() {
  const [doc, setDoc] = useState(DEFAULT_DOC);
  const [workflows, setWorkflows] = useState(() => [emptyWorkflow("main")]);
  const [active, setActive] = useState(0);
  const [selected, setSelected] = useState(null);
  const [showYaml, setShowYaml] = useState(false);
  const [importText, setImportText] = useState("");
  const [importing, setImporting] = useState(false);
  const [error, setError] = useState("");
  const fileRef = useRef(null);

  const wf = workflows[active] || workflows[0];
  const nodes = wf?.nodes || [];
  const edges = wf?.edges || [];

  // Write a mutation back into the active workflow.
  const updateActive = useCallback(
    (patch) => {
      setWorkflows((ws) => ws.map((w, i) => (i === active ? { ...w, ...patch } : w)));
    },
    [active]
  );

  const onNodesChange = useCallback(
    (changes) => updateActive({ nodes: applyNodeChanges(changes, wf.nodes) }),
    [wf, updateActive]
  );
  const onEdgesChange = useCallback(
    (changes) => updateActive({ edges: applyEdgeChanges(changes, wf.edges) }),
    [wf, updateActive]
  );
  const onConnect = useCallback(
    (conn) => {
      // Guard: a start node cannot depend on anything (no inbound edge).
      if (isStart(nodeById(wf.nodes, conn.target)?.data.kind)) {
        setError("a start node can't have a dependency (remove the target's start kind first)");
        return;
      }
      updateActive({ edges: addEdge({ ...conn, id: `e_${Date.now().toString(36)}` }, wf.edges) });
    },
    [wf, updateActive]
  );

  const addNode = useCallback(
    (kind) => {
      const id = newStepId(wf.nodes.map((n) => n.id), kind);
      const node = {
        id,
        type: "wf",
        position: { x: 120 + Math.random() * 240, y: 80 + Math.random() * 240 },
        data: { kind, spec: starterSpec(kind) },
      };
      updateActive({ nodes: [...wf.nodes, node] });
      setSelected(id);
    },
    [wf, updateActive]
  );

  const autoLayout = useCallback(() => {
    const next = wf.nodes.map((n) => ({ ...n }));
    layout(next, wf.edges);
    updateActive({ nodes: next });
  }, [wf, updateActive]);

  // ── property panel edits ──────────────────────────────────────────────────
  const selNode = selected ? nodeById(nodes, selected) : null;

  const setNodeKind = (kind) =>
    updateActive({
      nodes: nodes.map((n) => (n.id === selected ? { ...n, data: { ...n.data, kind } } : n)),
    });

  const setNodeId = (nextId) => {
    const clean = nextId.trim();
    if (!clean || nodes.some((n) => n.id === clean && n.id !== selected)) return;
    updateActive({
      nodes: nodes.map((n) => (n.id === selected ? { ...n, id: clean } : n)),
      edges: edges.map((e) => ({
        ...e,
        source: e.source === selected ? clean : e.source,
        target: e.target === selected ? clean : e.target,
      })),
    });
    setSelected(clean);
  };

  const setSpecField = (key, rawValue) => {
    const value = coerce(rawValue);
    updateActive({
      nodes: nodes.map((n) =>
        n.id === selected ? { ...n, data: { ...n.data, spec: { ...n.data.spec, [key]: value } } } : n
      ),
    });
  };
  const removeSpecField = (key) =>
    updateActive({
      nodes: nodes.map((n) => {
        if (n.id !== selected) return n;
        const spec = { ...n.data.spec };
        delete spec[key];
        return { ...n, data: { ...n.data, spec } };
      }),
    });
  const deleteNode = () => {
    updateActive({
      nodes: nodes.filter((n) => n.id !== selected),
      edges: edges.filter((e) => e.source !== selected && e.target !== selected),
    });
    setSelected(null);
  };

  // ── workflow tabs ───────────────────────────────────────────────────────────
  const addWorkflow = () => {
    setWorkflows((ws) => [...ws, emptyWorkflow(`workflow_${ws.length + 1}`)]);
    setActive(workflows.length);
    setSelected(null);
  };
  const renameWorkflow = (name) => updateActive({ name: name.replace(/\s+/g, "_") });
  const deleteWorkflow = () => {
    if (workflows.length === 1) return;
    setWorkflows((ws) => ws.filter((_, i) => i !== active));
    setActive((a) => Math.max(0, a - 1));
    setSelected(null);
  };

  // ── import / export ─────────────────────────────────────────────────────────
  const yamlText = useMemo(() => {
    try {
      return serializeConfig(doc, workflows);
    } catch (e) {
      return `# export error: ${e.message}`;
    }
  }, [doc, workflows]);

  const doImport = (text) => {
    try {
      const { doc: d, workflows: ws } = parseConfig(text);
      setDoc(d);
      setWorkflows(ws);
      setActive(0);
      setSelected(null);
      setImporting(false);
      setError("");
    } catch (e) {
      setError(`import failed: ${e.message}`);
    }
  };
  const onFile = (ev) => {
    const f = ev.target.files?.[0];
    if (!f) return;
    const r = new FileReader();
    r.onload = () => doImport(String(r.result));
    r.readAsText(f);
  };
  const download = () => {
    const blob = new Blob([yamlText], { type: "text/yaml" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${(doc.agent?.name || "agentd") + "-workflows"}.yaml`;
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="flex h-[calc(100vh-3.25rem)] flex-col">
      {/* toolbar */}
      <div className="flex flex-wrap items-center gap-2 border-b border-[var(--line)] px-3 py-2 text-sm">
        <div className="flex items-center gap-1">
          {workflows.map((w, i) => (
            <button
              key={w.id}
              onClick={() => {
                setActive(i);
                setSelected(null);
              }}
              className={`rounded px-2 py-1 font-mono text-xs ${
                i === active
                  ? "bg-[var(--panel)] text-[var(--fg-strong)] ring-1 ring-[var(--green)]"
                  : "text-[var(--dim)] hover:text-[var(--fg-strong)]"
              }`}
            >
              {w.name}
            </button>
          ))}
          <button
            onClick={addWorkflow}
            title="add a workflow"
            className="rounded px-1.5 py-1 text-[var(--dim)] hover:text-[var(--green)]"
          >
            ＋
          </button>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <button onClick={autoLayout} className="btn-ghost">auto-layout</button>
          <button onClick={() => setShowYaml((v) => !v)} className="btn-ghost">
            {showYaml ? "hide yaml" : "yaml"}
          </button>
          <button onClick={() => setImporting(true)} className="btn-ghost">import</button>
          <button onClick={download} className="btn-solid">export ↓</button>
        </div>
      </div>

      {error && (
        <div className="border-b border-red-900/50 bg-red-950/40 px-3 py-1.5 text-xs text-red-300">
          {error}
          <button onClick={() => setError("")} className="ml-2 underline">dismiss</button>
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        {/* palette */}
        <aside className="w-48 shrink-0 overflow-y-auto border-r border-[var(--line)] p-2 text-xs">
          {CATEGORIES.map((c) => (
            <div key={c.id} className="mb-3">
              <div className="mb-1 flex items-center gap-1.5">
                <span className="h-2 w-2 rounded-full" style={{ background: c.accent }} />
                <span className="font-semibold text-[var(--fg-strong)]">{c.label}</span>
              </div>
              <div className="flex flex-wrap gap-1">
                {kindsInCategory(c.id).map((k) => (
                  <button
                    key={k}
                    onClick={() => addNode(k)}
                    title={BLURBS[k] || (nodeInfo(k).implemented ? k : `${k} (not yet implemented)`)}
                    className="rounded border border-[var(--line)] px-1.5 py-0.5 font-mono text-[11px] text-[var(--fg)] hover:border-[color:var(--dim)] hover:text-[var(--fg-strong)]"
                    style={{ opacity: nodeInfo(k).implemented ? 1 : 0.5 }}
                  >
                    {k}
                  </button>
                ))}
              </div>
            </div>
          ))}
        </aside>

        {/* canvas */}
        <div className="relative min-w-0 flex-1">
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={NODE_TYPES}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            onNodeClick={(_, n) => setSelected(n.id)}
            onPaneClick={() => setSelected(null)}
            fitView
            proOptions={{ hideAttribution: true }}
            defaultEdgeOptions={{ animated: false, style: { stroke: "var(--dimmer)" } }}
          >
            <Background color="var(--line)" gap={20} />
            <Controls className="!bg-[var(--panel)]" />
            <MiniMap
              pannable
              zoomable
              nodeColor={(n) => accentFor(n.data?.kind)}
              maskColor="rgba(0,0,0,0.6)"
              className="!bg-[var(--panel)]"
            />
          </ReactFlow>

          {showYaml && (
            <div className="absolute right-2 top-2 bottom-2 w-[38%] overflow-auto rounded-md border border-[var(--line)] bg-[var(--bg-soft)] p-2">
              <div className="mb-1 text-[11px] text-[var(--dim)]">config preview (live)</div>
              <pre className="whitespace-pre-wrap font-mono text-[11px] leading-relaxed text-[var(--fg)]">
                {yamlText}
              </pre>
            </div>
          )}
        </div>

        {/* property panel */}
        <aside className="w-72 shrink-0 overflow-y-auto border-l border-[var(--line)] p-3 text-sm">
          {selNode ? (
            <PropertyPanel
              node={selNode}
              onId={setNodeId}
              onKind={setNodeKind}
              onField={setSpecField}
              onRemoveField={removeSpecField}
              onDelete={deleteNode}
            />
          ) : (
            <WorkflowPanel
              wf={wf}
              doc={doc}
              onRename={renameWorkflow}
              onDelete={deleteWorkflow}
              canDelete={workflows.length > 1}
              onMeta={(meta) => updateActive({ meta })}
            />
          )}
        </aside>
      </div>

      {/* import modal */}
      {importing && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4" onClick={() => setImporting(false)}>
          <div className="w-full max-w-2xl rounded-lg border border-[var(--line)] bg-[var(--panel)] p-4" onClick={(e) => e.stopPropagation()}>
            <div className="mb-2 flex items-center justify-between">
              <h3 className="font-semibold text-[var(--fg-strong)]">Import a config</h3>
              <button onClick={() => fileRef.current?.click()} className="btn-ghost">choose file…</button>
              <input ref={fileRef} type="file" accept=".yaml,.yml,.json" onChange={onFile} className="hidden" />
            </div>
            <textarea
              value={importText}
              onChange={(e) => setImportText(e.target.value)}
              placeholder={'paste a config_version: "2" document…'}
              className="h-72 w-full rounded border border-[var(--line)] bg-[var(--bg-soft)] p-2 font-mono text-xs text-[var(--fg)]"
            />
            <div className="mt-2 flex justify-end gap-2">
              <button onClick={() => setImporting(false)} className="btn-ghost">cancel</button>
              <button onClick={() => doImport(importText)} className="btn-solid">load</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ── panels ──────────────────────────────────────────────────────────────────
function PropertyPanel({ node, onId, onKind, onField, onRemoveField, onDelete }) {
  const info = nodeInfo(node.data.kind);
  const spec = node.data.spec || {};
  const known = info.fields || [];
  const unset = known.filter((f) => !(f in spec));
  const [newField, setNewField] = useState("");

  return (
    <div>
      <div className="mb-3 flex items-center justify-between">
        <span className="font-semibold text-[var(--fg-strong)]">Step</span>
        <button onClick={onDelete} className="text-xs text-red-400 hover:text-red-300">delete</button>
      </div>

      <Label>id</Label>
      <input
        defaultValue={node.id}
        key={node.id}
        onBlur={(e) => onId(e.target.value)}
        className="input font-mono"
      />

      <Label>kind</Label>
      <select value={node.data.kind} onChange={(e) => onKind(e.target.value)} className="input font-mono">
        {CATEGORIES.map((c) => (
          <optgroup key={c.id} label={c.label}>
            {kindsInCategory(c.id).map((k) => (
              <option key={k} value={k}>
                {k}
                {NODES[k]?.implemented ? "" : " (soon)"}
              </option>
            ))}
          </optgroup>
        ))}
      </select>
      {BLURBS[node.data.kind] && (
        <p className="mt-1 text-[11px] text-[var(--dim)]">{BLURBS[node.data.kind]}</p>
      )}

      <div className="mt-4 mb-1 flex items-center justify-between">
        <span className="text-xs font-semibold text-[var(--fg-strong)]">Fields</span>
        {info.required?.length > 0 && (
          <span className="text-[10px] text-[var(--dim)]">required: {info.required.join(", ")}</span>
        )}
      </div>

      {Object.keys(spec).length === 0 && (
        <p className="text-[11px] text-[var(--dim)]">no fields set — add one below.</p>
      )}
      {Object.entries(spec).map(([k, v]) => (
        <div key={k} className="mb-2">
          <div className="flex items-center justify-between">
            <Label required={info.required?.includes(k)}>{k}</Label>
            <button onClick={() => onRemoveField(k)} className="text-[10px] text-[var(--dim)] hover:text-red-400">
              ×
            </button>
          </div>
          <FieldInput value={v} onChange={(val) => onField(k, val)} />
        </div>
      ))}

      <div className="mt-3 flex gap-1">
        <select value={newField} onChange={(e) => setNewField(e.target.value)} className="input flex-1 text-xs">
          <option value="">＋ add field…</option>
          {unset.map((f) => (
            <option key={f} value={f}>
              {f}
              {info.required?.includes(f) ? " *" : ""}
            </option>
          ))}
          <option value="__custom__">custom…</option>
        </select>
        <button
          onClick={() => {
            const f = newField === "__custom__" ? prompt("field name") : newField;
            if (f) onField(f, "");
            setNewField("");
          }}
          className="btn-ghost"
        >
          add
        </button>
      </div>
    </div>
  );
}

function WorkflowPanel({ wf, doc, onRename, onDelete, canDelete, onMeta }) {
  return (
    <div>
      <div className="mb-3 flex items-center justify-between">
        <span className="font-semibold text-[var(--fg-strong)]">Workflow</span>
        {canDelete && (
          <button onClick={onDelete} className="text-xs text-red-400 hover:text-red-300">delete</button>
        )}
      </div>
      <Label>name</Label>
      <input defaultValue={wf.name} key={wf.id} onBlur={(e) => onRename(e.target.value)} className="input font-mono" />

      <Label>description</Label>
      <input
        defaultValue={wf.meta?.description || ""}
        key={`${wf.id}-desc`}
        onBlur={(e) => onMeta({ ...wf.meta, description: e.target.value || undefined })}
        className="input"
      />

      <p className="mt-4 text-[11px] leading-relaxed text-[var(--dim)]">
        Click a node to edit its fields. Drag from a node's right edge to another's left edge to add a{" "}
        <span className="font-mono text-[var(--fg)]">depends_on</span> edge. Use the palette to add steps; a{" "}
        <span style={{ color: "#4ade80" }}>trigger</span> node starts runs.
      </p>
      <div className="mt-3 rounded border border-[var(--line)] bg-[var(--bg-soft)] p-2 text-[11px] text-[var(--dim)]">
        <div>{wf.nodes.length} steps · {wf.edges.length} edges</div>
        {!doc.config_version && <div className="mt-1 text-amber-400">no config_version — export adds it on import only</div>}
      </div>
    </div>
  );
}

// ── small inputs ──────────────────────────────────────────────────────────────
function Label({ children, required }) {
  return (
    <label className="mb-0.5 mt-2 block text-[11px] text-[var(--dim)]">
      {children}
      {required && <span className="text-red-400"> *</span>}
    </label>
  );
}
function FieldInput({ value, onChange }) {
  const isComplex = value !== null && typeof value === "object";
  if (isComplex) {
    return (
      <textarea
        defaultValue={JSON.stringify(value, null, 2)}
        onBlur={(e) => onChange(e.target.value)}
        className="input h-24 font-mono text-[11px]"
      />
    );
  }
  return (
    <input
      defaultValue={value === true || value === false ? String(value) : value ?? ""}
      key={JSON.stringify(value)}
      onBlur={(e) => onChange(e.target.value)}
      className="input font-mono text-xs"
    />
  );
}

// Coerce a text field value to a JSON type when it parses, else keep the string.
function coerce(raw) {
  if (typeof raw !== "string") return raw;
  const t = raw.trim();
  if (t === "") return "";
  if (t === "true") return true;
  if (t === "false") return false;
  if ((t.startsWith("{") || t.startsWith("[")) ) {
    try {
      return JSON.parse(t);
    } catch {
      /* keep as string */
    }
  }
  if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
  return raw;
}
function nodeById(nodes, id) {
  return nodes.find((n) => n.id === id) || null;
}

export default function WorkflowEditor() {
  return (
    <ReactFlowProvider>
      <Editor />
    </ReactFlowProvider>
  );
}
