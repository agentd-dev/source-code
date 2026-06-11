"use client";

import { useState } from "react";

// A small real-shaped sample so the page works without a run handy.
const SAMPLE = JSON.stringify(
  {
    workflow: "doc_classifier",
    start_node: "main",
    execution_id: "exec-6a2a0f2d-2a4fb7-1",
    status: "completed",
    last_node: "done",
    detail: null,
    wall_ms: 3,
    cost: { llm_calls: 1, llm_tokens: 128, node_executions: 3, policy_denials: 0 },
    trace: {
      execution_id: "exec-6a2a0f2d-2a4fb7-1",
      entries: [
        {
          node_id: "classify",
          kind: "llm_infer",
          outcome: "continue",
          branch: "invoice",
          output: { content: '{"label":"invoice","confidence":0.97}', parsed: { label: "invoice", confidence: 0.97 } },
          elapsed_ms: 2,
        },
        { node_id: "route", kind: "switch", outcome: "continue", branch: "invoice", output: null, elapsed_ms: 0 },
        { node_id: "done", kind: "terminate", outcome: "terminate", branch: null, output: null, elapsed_ms: 0 },
      ],
    },
  },
  null,
  2,
);

const STATUS = {
  completed: "ok",
  failed: "fail",
  timed_out: "timeout",
  errored: "errored",
  paused: "paused",
};

function num(v) {
  return typeof v === "number" ? v : 0;
}

function preview(value) {
  if (value === null || value === undefined) return null;
  const s = JSON.stringify(value);
  return s.length > 280 ? s.slice(0, 280) + "…" : s;
}

export default function Inspect() {
  const [text, setText] = useState("");
  const [record, setRecord] = useState(null);
  const [error, setError] = useState("");

  function load(raw) {
    setText(raw);
    if (!raw.trim()) {
      setRecord(null);
      setError("");
      return;
    }
    try {
      setRecord(JSON.parse(raw));
      setError("");
    } catch (e) {
      setRecord(null);
      setError(String(e.message || e));
    }
  }

  function onFile(e) {
    const file = e.target.files?.[0];
    if (!file) return;
    file.text().then(load);
  }

  const cost = record?.cost ?? {};
  const entries = record?.trace?.entries ?? [];

  return (
    <main className="mx-auto max-w-5xl px-4 py-10">
      <h1 className="text-3xl font-bold text-[var(--accent)]">run inspector</h1>
      <p className="mt-3 max-w-2xl text-[var(--fg)]">
        Paste or upload a run record written by{" "}
        <code className="text-[var(--accent)]">agentd --record run.json</code> — the same JSON{" "}
        <code className="text-[var(--accent)]">agentd inspect</code> renders in the terminal. Runs
        entirely in your browser; nothing is uploaded.
      </p>

      <div className="mt-6 flex flex-wrap items-center gap-3 text-sm">
        <button
          onClick={() => load(SAMPLE)}
          className="frame px-3 py-1 text-[var(--accent)] hover:bg-[var(--line)]"
        >
          load sample
        </button>
        <label className="frame cursor-pointer px-3 py-1 text-[var(--dim)] hover:text-[var(--accent)]">
          upload run.json
          <input type="file" accept="application/json,.json" onChange={onFile} className="hidden" />
        </label>
        {record && (
          <button
            onClick={() => load("")}
            className="text-[var(--dim)] hover:text-[var(--accent)]"
          >
            clear
          </button>
        )}
      </div>

      <textarea
        value={text}
        onChange={(e) => load(e.target.value)}
        placeholder="{ … run record JSON … }"
        spellCheck={false}
        className="frame mt-4 h-40 w-full resize-y bg-[var(--panel)] p-3 font-mono text-xs text-[var(--fg)] outline-none"
      />

      {error && <p className="mt-3 text-sm text-[var(--accent-dim)]">invalid JSON: {error}</p>}

      {record && (
        <div className="frame mt-6">
          <div className="frame-title">
            <span className="dot" />
            <span className="dot" />
            <span className="dot" />
            <span>agentd — inspect {record.execution_id || "?"}</span>
          </div>
          <div className="p-4 text-sm leading-7">
            <div>
              <span className="text-[var(--dim)]">workflow</span>{" "}
              <span className="text-[var(--accent)]">{record.workflow}</span>
              <span className="text-[var(--dim)]"> · status </span>
              <span className="text-[var(--accent)]">
                {STATUS[record.status] || record.status}
              </span>
            </div>
            <div className="text-[var(--dim)]">
              start {record.start_node} · {num(record.wall_ms)} ms ·{" "}
              {num(cost.llm_calls)} llm call(s) / {num(cost.llm_tokens)} tokens ·{" "}
              {num(cost.policy_denials)} policy denial(s)
            </div>

            <ol className="mt-4 space-y-2">
              {entries.length === 0 && (
                <li className="text-[var(--dim)]">(no node trace captured)</li>
              )}
              {entries.map((e, i) => {
                const out = preview(e.output);
                return (
                  <li key={i} className="border-l-2 border-[var(--line)] pl-3">
                    <div>
                      <span className="text-[var(--dim)]">{String(i + 1).padStart(2, "0")}.</span>{" "}
                      <span className="text-[var(--accent)]">{e.node_id}</span>{" "}
                      <span className="text-[var(--dim)]">[{e.kind}]</span>{" "}
                      <span className="text-[var(--fg)]">{e.outcome}</span>
                      {e.branch && <span className="text-[var(--accent-dim)]"> →{e.branch}</span>}
                      <span className="text-[var(--dim)]"> · {num(e.elapsed_ms)} ms</span>
                    </div>
                    {out && (
                      <pre className="mt-1 overflow-x-auto whitespace-pre-wrap break-words text-xs text-[var(--accent-dim)]">
                        {out}
                      </pre>
                    )}
                  </li>
                );
              })}
            </ol>

            {record.detail != null && (
              <div className="mt-4 text-[var(--dim)]">
                outcome:{" "}
                <span className="text-[var(--accent-dim)]">{preview(record.detail)}</span>
              </div>
            )}
          </div>
        </div>
      )}
    </main>
  );
}
