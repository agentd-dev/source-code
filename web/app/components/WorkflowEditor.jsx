"use client";

// A visual editor for agentd workflows. Nodes are steps; edges are the
// `depends_on` relation, so the graph on screen IS the dependency graph the
// runtime walks — an edge drawn here is a `depends_on` entry written out, and a
// cycle is therefore not expressible. The palette and property forms are driven by the node
// registry generated from `agentd --workflow-schema`. Import a YAML config to
// edit it, export the whole document back out. Multi-workflow: a config can hold
// several workflows, switched by the tabs.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { validateWorkflow, canConnect } from "../../lib/validate";

const DEFAULT_DOC = { config_version: "1" };

// ── custom node ─────────────────────────────────────────────────────────────
function WfNode({ id, data, selected }) {
  const info = nodeInfo(data.kind);
  const accent = accentFor(data.kind);
  const start = isStart(data.kind);
  return (
    <div
      style={{
        borderColor: data.invalid ? "#ef4444" : selected ? accent : "var(--line)",
        boxShadow: selected ? `0 0 0 1px ${data.invalid ? "#ef4444" : accent}` : "none",
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
        {data.invalid && (
          <span className="ml-auto text-[11px] leading-none text-red-400" title="this step has errors">
            ✗
          </span>
        )}
        {!info.implemented && !data.invalid && (
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
  const [selectedEdge, setSelectedEdge] = useState(null);
  const [showIssues, setShowIssues] = useState(true);
  const [showYaml, setShowYaml] = useState(false);
  const [importText, setImportText] = useState("");
  const [importing, setImporting] = useState(false);
  const [paletteQuery, setPaletteQuery] = useState("");
  // The node a connection is being dragged FROM, while the drag is in flight.
  const [connectingFrom, setConnectingFrom] = useState(null);
  const [error, setError] = useState("");
  const fileRef = useRef(null);

  // ── history ────────────────────────────────────────────────────────────────
  // An editor without undo is a trap: every experiment costs the fear of not
  // getting back. The snapshot is the whole workflow array, which is small
  // (a config is tens of nodes, not thousands) and makes correctness trivial —
  // no per-action inverse to get wrong.
  //
  // Dragging is the one thing that must NOT flood the stack: a single drag
  // fires a position change per frame. `commit` coalesces by tag, so a whole
  // drag collapses into one entry a person can undo in one keystroke.
  const past = useRef([]);
  const future = useRef([]);
  const lastTag = useRef(null);
  const [histVersion, setHistVersion] = useState(0);

  const commit = useCallback(
    (tag) => {
      // Coalesce consecutive same-tag mutations (a drag, a text field) into one.
      if (tag && tag === lastTag.current) return;
      past.current = [...past.current.slice(-49), workflows];
      future.current = [];
      lastTag.current = tag || null;
      setHistVersion((v) => v + 1);
    },
    [workflows]
  );
  const undo = useCallback(() => {
    if (!past.current.length) return;
    const prev = past.current[past.current.length - 1];
    past.current = past.current.slice(0, -1);
    future.current = [workflows, ...future.current];
    lastTag.current = null;
    setWorkflows(prev);
    setHistVersion((v) => v + 1);
  }, [workflows]);
  const redo = useCallback(() => {
    if (!future.current.length) return;
    const next = future.current[0];
    future.current = future.current.slice(1);
    past.current = [...past.current, workflows];
    lastTag.current = null;
    setWorkflows(next);
    setHistVersion((v) => v + 1);
  }, [workflows]);

  const wf = workflows[active] || workflows[0];
  const nodes = wf?.nodes || [];
  const edges = wf?.edges || [];

  // Write a mutation back into the active workflow. `tag` groups a burst of
  // related changes into one undo entry; omit it for a discrete action.
  const updateActive = useCallback(
    (patch, tag) => {
      commit(tag);
      setWorkflows((ws) => ws.map((w, i) => (i === active ? { ...w, ...patch } : w)));
    },
    [active, commit]
  );

  const onNodesChange = useCallback(
    (changes) => {
      // A drag emits a position change per frame; selection changes are not
      // edits at all. Tagging the first as one unit and skipping the second
      // keeps the undo stack made of things a person actually did.
      const onlySelection = changes.every((c) => c.type === "select");
      const dragging = changes.some((c) => c.type === "position" && c.dragging);
      if (onlySelection) {
        setWorkflows((ws) =>
          ws.map((w, i) => (i === active ? { ...w, nodes: applyNodeChanges(changes, w.nodes) } : w))
        );
        return;
      }
      updateActive({ nodes: applyNodeChanges(changes, wf.nodes) }, dragging ? "drag" : null);
    },
    [wf, updateActive, active]
  );
  const onEdgesChange = useCallback(
    (changes) => updateActive({ edges: applyEdgeChanges(changes, wf.edges) }),
    [wf, updateActive]
  );
  // Whether an edge is legal at all — asked while the user is still dragging,
  // so an impossible connection is refused at the handle rather than accepted
  // and then reported as an error afterwards.
  const isValidConnection = useCallback(
    (conn) => {
      const s = nodeById(wf.nodes, conn.source)?.data.kind;
      const t = nodeById(wf.nodes, conn.target)?.data.kind;
      if (wf.edges.some((e) => e.source === conn.source && e.target === conn.target)) return false;
      return canConnect(s, t, conn.source, conn.target).ok;
    },
    [wf]
  );

  // React Flow refuses an invalid connection SILENTLY — `isValidConnection`
  // returns false and `onConnect` never fires, so the reason we already
  // computed is never seen. Tracking the drag lets the canvas dim what cannot
  // accept the edge while the user is still deciding, and lets a refused drop
  // explain itself instead of just not happening.
  const onConnectStart = useCallback((_e, { nodeId }) => {
    setConnectingFrom(nodeId);
    setError("");
  }, []);

  const onConnectEnd = useCallback(
    (e) => {
      const from = connectingFrom;
      setConnectingFrom(null);
      if (!from) return;
      // Dropped on a node that refused it: say why, once, in the same place
      // every other error appears.
      const el = e.target?.closest?.("[data-id]");
      const toId = el?.getAttribute?.("data-id");
      if (!toId || toId === from) return;
      const s = nodeById(wf.nodes, from)?.data.kind;
      const t = nodeById(wf.nodes, toId)?.data.kind;
      const verdict = canConnect(s, t, from, toId);
      if (!verdict.ok) setError(verdict.why);
      else if (wf.edges.some((x) => x.source === from && x.target === toId))
        setError(`${toId} already depends on ${from}`);
    },
    [connectingFrom, wf]
  );

  const onConnect = useCallback(
    (conn) => {
      const s = nodeById(wf.nodes, conn.source)?.data.kind;
      const t = nodeById(wf.nodes, conn.target)?.data.kind;
      const verdict = canConnect(s, t, conn.source, conn.target);
      if (!verdict.ok) {
        setError(verdict.why);
        return;
      }
      if (wf.edges.some((e) => e.source === conn.source && e.target === conn.target)) {
        setError(`${conn.target} already depends on ${conn.source}`);
        return;
      }
      updateActive({ edges: addEdge({ ...conn, id: `e_${Date.now().toString(36)}` }, wf.edges) });
      setSelectedEdge(null);
    },
    [wf, updateActive]
  );

  // ── edges as first-class objects ────────────────────────────────────────
  const retargetEdge = (edgeId, field, value) => {
    const edge = wf.edges.find((e) => e.id === edgeId);
    if (!edge) return;
    const next = { ...edge, [field]: value };
    const s = nodeById(wf.nodes, next.source)?.data.kind;
    const tk = nodeById(wf.nodes, next.target)?.data.kind;
    const verdict = canConnect(s, tk, next.source, next.target);
    if (!verdict.ok) {
      setError(verdict.why);
      return;
    }
    updateActive({ edges: wf.edges.map((e) => (e.id === edgeId ? next : e)) });
  };
  const setEdgeLabel = (edgeId, label) =>
    updateActive({
      edges: wf.edges.map((e) => (e.id === edgeId ? { ...e, label: label || undefined } : e)),
    });
  const deleteEdge = (edgeId) => {
    updateActive({ edges: wf.edges.filter((e) => e.id !== edgeId) });
    setSelectedEdge(null);
  };

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

  // Duplicate the selection, offset so it is visibly a copy rather than
  // something that vanished under the original. Edges are deliberately NOT
  // copied: a duplicated step almost always wants new wiring, and inheriting
  // the original's dependencies silently is worse than re-drawing two edges.
  const duplicateNode = useCallback(() => {
    const src = nodeById(wf.nodes, selected);
    if (!src) return;
    const id = newStepId(wf.nodes.map((n) => n.id), src.data.kind);
    const copy = {
      ...src,
      id,
      position: { x: src.position.x + 40, y: src.position.y + 40 },
      selected: false,
      data: { ...src.data, spec: JSON.parse(JSON.stringify(src.data.spec ?? {})) },
    };
    updateActive({ nodes: [...wf.nodes, copy] });
    setSelected(id);
  }, [wf, selected, updateActive]);

  // Match on the kind AND its blurb, so a search for what you want to do finds
  // the node even when you do not know its name.
  const paletteMatches = useMemo(() => {
    const q = paletteQuery.trim().toLowerCase();
    const all = CATEGORIES.flatMap((c) => kindsInCategory(c.id));
    if (!q) return all;
    return all.filter(
      (k) => k.toLowerCase().includes(q) || (BLURBS[k] || "").toLowerCase().includes(q)
    );
  }, [paletteQuery]);

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

  // ── keyboard ────────────────────────────────────────────────────────────────
  // The bindings people already know from n8n and every canvas tool, so nobody
  // has to learn ours. Guarded on the event target: a Delete pressed inside a
  // property field must delete a character, not the step being edited — that
  // mistake is unrecoverable-feeling even with undo, because the field loses
  // focus and the panel disappears.
  useEffect(() => {
    const typing = (el) =>
      el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);
    const onKey = (e) => {
      if (typing(e.target)) return;
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === "z") {
        e.preventDefault();
        e.shiftKey ? redo() : undo();
        return;
      }
      if (mod && e.key.toLowerCase() === "y") {
        e.preventDefault();
        redo();
        return;
      }
      if (mod && e.key.toLowerCase() === "d") {
        e.preventDefault();
        duplicateNode();
        return;
      }
      if (e.key === "Delete" || e.key === "Backspace") {
        if (selectedEdge) {
          e.preventDefault();
          deleteEdge(selectedEdge);
        } else if (selected) {
          e.preventDefault();
          deleteNode();
        }
        return;
      }
      if (e.key === "l" || e.key === "L") {
        e.preventDefault();
        autoLayout();
        return;
      }
      if (e.key === "Escape") {
        setSelected(null);
        setSelectedEdge(null);
        setError("");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [undo, redo, duplicateNode, autoLayout, selected, selectedEdge, deleteEdge, deleteNode]);


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
  // Live validation — the same rules the runtime applies, so a graph the
  // editor calls clean is one `agentd --validate-config` also accepts.
  const issues = useMemo(() => validateWorkflow(wf), [wf]);
  const invalid = new Set(issues.errors.filter((e) => e.id).map((e) => e.id));

  const download = () => {
    // Validate before anything leaves the editor: exporting a graph the
    // daemon would refuse at startup is the one outcome worth blocking.
    if (issues.errors.length) {
      setShowIssues(true);
      setError(
        `${issues.errors.length} problem${issues.errors.length > 1 ? "s" : ""} to fix before export — see the checks panel`,
      );
      return;
    }
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
          <button
            onClick={() => setShowIssues((v) => !v)}
            className="btn-ghost"
            title="graph checks — the rules the runtime enforces"
            style={
              issues.errors.length
                ? { color: "#f87171", borderColor: "rgba(248,113,113,.5)" }
                : { color: "var(--green)", borderColor: "color-mix(in srgb, var(--green) 45%, transparent)" }
            }
          >
            {issues.errors.length
              ? `${issues.errors.length} error${issues.errors.length > 1 ? "s" : ""}`
              : issues.warnings.length
                ? `valid · ${issues.warnings.length} note${issues.warnings.length > 1 ? "s" : ""}`
                : "valid"}
          </button>
          {/* Undo is the affordance that makes every other one safe to try, so it
              is visible rather than keyboard-only — and it shows whether there
              is anything to undo, which is the question people actually have. */}
          <button
            onClick={undo}
            disabled={!past.current.length}
            title="undo (⌘Z)"
            className="btn-ghost disabled:opacity-40"
          >
            undo
          </button>
          <button
            onClick={redo}
            disabled={!future.current.length}
            title="redo (⇧⌘Z)"
            className="btn-ghost disabled:opacity-40"
          >
            redo
          </button>
          <button onClick={autoLayout} className="btn-ghost" title="auto-layout (L)">auto-layout</button>
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
          {/* Sixty-seven kinds is too many to scan. Searching the NAME and the
              blurb means "http" finds the http node and "wait" also finds
              `join` — you look for what you want to do, not for what we named
              it. Enter adds the single remaining match, so the fast path is
              type-three-letters-Enter without touching the mouse. */}
          <input
            value={paletteQuery}
            onChange={(e) => setPaletteQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && paletteMatches.length === 1) {
                addNode(paletteMatches[0]);
                setPaletteQuery("");
              }
              if (e.key === "Escape") setPaletteQuery("");
            }}
            placeholder="search nodes…"
            aria-label="search nodes"
            className="mb-3 w-full rounded border border-[var(--line)] bg-[var(--bg)] px-2 py-1 font-mono text-[11px] text-[var(--fg)] placeholder:text-[var(--dimmer)]"
          />
          {paletteQuery && paletteMatches.length === 0 && (
            <p className="mb-3 text-[11px] text-[var(--dim)]">
              nothing matches “{paletteQuery}”
            </p>
          )}
          {CATEGORIES.map((c) => {
            const kinds = kindsInCategory(c.id).filter((k) => paletteMatches.includes(k));
            if (!kinds.length) return null;
            return (
              <div key={c.id} className="mb-3">
                <div className="mb-1 flex items-center gap-1.5">
                  <span className="h-2 w-2 rounded-full" style={{ background: c.accent }} />
                  <span className="font-semibold text-[var(--fg-strong)]">{c.label}</span>
                </div>
                <div className="flex flex-wrap gap-1">
                  {kinds.map((k) => (
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
            );
          })}
          <div className="mt-4 border-t border-[var(--line)] pt-3 text-[10px] leading-relaxed text-[var(--dim)]">
            <div className="mb-1 font-semibold text-[var(--fg)]">shortcuts</div>
            <div className="flex justify-between"><span>undo / redo</span><span className="font-mono">⌘Z ⇧⌘Z</span></div>
            <div className="flex justify-between"><span>duplicate step</span><span className="font-mono">⌘D</span></div>
            <div className="flex justify-between"><span>delete selection</span><span className="font-mono">Del</span></div>
            <div className="flex justify-between"><span>auto-layout</span><span className="font-mono">L</span></div>
            <div className="flex justify-between"><span>deselect</span><span className="font-mono">Esc</span></div>
          </div>
        </aside>

        {/* canvas */}
        <div className="relative min-w-0 flex-1">
          <ReactFlow
            nodes={nodes.map((n) => {
              const data = invalid.has(n.id) ? { ...n.data, invalid: true } : n.data;
              // While a connection is in flight, fade every node that cannot
              // be its target. Showing the legal landing places beats letting
              // someone drag at a node that will silently refuse.
              if (!connectingFrom || n.id === connectingFrom) return { ...n, data };
              const sk = nodeById(nodes, connectingFrom)?.data.kind;
              const ok =
                canConnect(sk, n.data.kind, connectingFrom, n.id).ok &&
                !edges.some((e) => e.source === connectingFrom && e.target === n.id);
              return { ...n, data, style: { ...n.style, opacity: ok ? 1 : 0.35 } };
            })}
            edges={edges.map((e) => ({
              ...e,
              selected: e.id === selectedEdge,
              style:
                e.id === selectedEdge
                  ? { stroke: "var(--green)", strokeWidth: 2 }
                  : { stroke: "var(--dimmer)" },
            }))}
            nodeTypes={NODE_TYPES}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            onConnectStart={onConnectStart}
            onConnectEnd={onConnectEnd}
            isValidConnection={isValidConnection}
            onNodeClick={(_, n) => {
              setSelected(n.id);
              setSelectedEdge(null);
            }}
            onEdgeClick={(_, e) => {
              setSelectedEdge(e.id);
              setSelected(null);
            }}
            onPaneClick={() => {
              setSelected(null);
              setSelectedEdge(null);
            }}
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

          {showIssues && (issues.errors.length > 0 || issues.warnings.length > 0) && (
            <div className="absolute bottom-2 left-2 right-2 max-h-[38%] overflow-auto rounded-md border border-[var(--line)] bg-[var(--panel)] p-2 text-xs">
              <div className="mb-1 flex items-center justify-between">
                <span className="font-mono text-[11px] text-[var(--dim)]">
                  graph checks — the rules the runtime enforces
                </span>
                <button onClick={() => setShowIssues(false)} className="text-[var(--dim)] hover:text-[var(--fg-strong)]">
                  ×
                </button>
              </div>
              <ul className="space-y-1">
                {issues.errors.map((e, i) => (
                  <li key={`e${i}`} className="flex gap-2">
                    <span className="text-red-400">✗</span>
                    <button
                      className="text-left text-[var(--fg)] hover:underline"
                      onClick={() => e.id && (setSelected(e.id), setSelectedEdge(null))}
                    >
                      {e.message}
                    </button>
                  </li>
                ))}
                {issues.warnings.map((w, i) => (
                  <li key={`w${i}`} className="flex gap-2">
                    <span className="text-amber-400">!</span>
                    <button
                      className="text-left text-[var(--dim)] hover:underline"
                      onClick={() => w.id && (setSelected(w.id), setSelectedEdge(null))}
                    >
                      {w.message}
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}

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
              issues={issues.errors.filter((e) => e.id === selNode.id)}
            />
          ) : selectedEdge ? (
            <EdgePanel
              edge={wf.edges.find((e) => e.id === selectedEdge)}
              nodes={wf.nodes}
              onRetarget={(field, value) => retargetEdge(selectedEdge, field, value)}
              onLabel={(v) => setEdgeLabel(selectedEdge, v)}
              onDelete={() => deleteEdge(selectedEdge)}
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
              placeholder={'paste a config_version: "1" document…'}
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

/**
 * An edge is `depends_on`: "target runs after source". It is worth inspecting
 * because re-pointing one is otherwise a delete-and-redraw, and because the
 * rules that govern it (no inbound edge on a start node, nothing after a
 * terminal step) are easier to learn when the editor states them.
 */
function EdgePanel({ edge, nodes, onRetarget, onLabel, onDelete }) {
  if (!edge) return null;
  const opts = nodes.map((n) => ({ id: n.id, kind: n.data.kind }));
  const src = nodes.find((n) => n.id === edge.source);
  const tgt = nodes.find((n) => n.id === edge.target);
  return (
    <div>
      <div className="mb-3">
        <div className="font-mono text-[11px] uppercase tracking-wider text-[var(--dim)]">
          connection
        </div>
        <div className="mt-1 text-sm text-[var(--fg-strong)]">
          <span className="font-mono">{edge.target}</span>{" "}
          <span className="text-[var(--dim)]">depends on</span>{" "}
          <span className="font-mono">{edge.source}</span>
        </div>
        <p className="mt-2 text-xs leading-relaxed text-[var(--dim)]">
          {tgt?.data.kind} runs only after {src?.data.kind} reaches a terminal state. Its output is
          addressable as{" "}
          <span className="kbd">{`{{steps.${edge.source}.output}}`}</span>.
        </p>
      </div>

      <Label>from (runs first)</Label>
      <select className="input" value={edge.source} onChange={(e) => onRetarget("source", e.target.value)}>
        {opts.map((o) => (
          <option key={o.id} value={o.id}>
            {o.id} · {o.kind}
          </option>
        ))}
      </select>

      <div className="mt-2" />
      <Label>to (runs after)</Label>
      <select className="input" value={edge.target} onChange={(e) => onRetarget("target", e.target.value)}>
        {opts.map((o) => (
          <option key={o.id} value={o.id}>
            {o.id} · {o.kind}
          </option>
        ))}
      </select>

      <div className="mt-2" />
      <Label>label (editor only)</Label>
      <input
        className="input"
        value={edge.label || ""}
        placeholder="e.g. on success"
        onChange={(e) => onLabel(e.target.value)}
      />

      <button onClick={onDelete} className="btn-ghost mt-4 w-full hover:!text-red-400">
        remove connection
      </button>
    </div>
  );
}

function PropertyPanel({ node, onId, onKind, onField, onRemoveField, onDelete, issues = [] }) {
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

      {issues.length > 0 && (
        <ul className="mb-3 space-y-1 rounded border border-red-500/40 bg-red-500/5 p-2 text-xs text-red-300">
          {issues.map((e, i) => (
            <li key={i}>{e.message.replace(/^step "[^"]+": /, "")}</li>
          ))}
        </ul>
      )}

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
