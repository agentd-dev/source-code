"use client";

import { useState } from "react";
import Console from "./Console";

/**
 * The console, with a run to choose from.
 *
 * One scripted demo shows what a run looks like; five show what agentd *is* —
 * that the same binary is a one-shot job, a daemon that wakes on an event, a
 * durable graph, a pair-programming session, and a thing you can hold to
 * account. Switching is the argument.
 *
 * Each script is text (~1 kB), so all five cost less than a screenshot and stay
 * selectable, searchable and theme-aware.
 */
const DEMOS = [
  {
    id: "one-shot",
    label: "one-shot",
    blurb: "Ask, answer, exit. The exit code is the terminal status.",
    title: "agentd — a one-shot job",
    script: [
      { t: "in", text: 'agentd --prompt "triage the newest issue and label it" \\' },
      { t: "out", text: "         --mcp github=https://mcp-github.internal/mcp \\" },
      { t: "out", text: "         --intelligence https://gateway.internal/v1" },
      { t: "out", text: "", ms: 220 },
      { t: "out", text: '{"event":"mcp.connect","server":"github","proto":"2026-07-28"}', cls: "comment" },
      { t: "out", text: '{"event":"run.start","tools":11,"servers":1}', cls: "comment" },
      { t: "spin", text: "thinking · 1.2k tok", ms: 1000 },
      { t: "out", text: '{"event":"tool.call","tool":"list_issues"}', cls: "comment" },
      { t: "spin", text: "list_issues · 0.4s", ms: 650 },
      { t: "out", text: '{"event":"tool.call","tool":"add_labels","args":{"labels":["bug"]}}', cls: "comment" },
      { t: "spin", text: "thinking · 2.6k tok", ms: 850 },
      { t: "out", text: "", ms: 120 },
      { t: "out", text: "Labelled #482 as bug: the stack trace shows a nil deref in" },
      { t: "out", text: "parse_config, not a usage error. Assigned to the api team." },
      { t: "out", text: "", ms: 120 },
      { t: "out", text: '{"event":"run.done","status":"completed","steps":4,"exit_code":0}', cls: "comment" },
      { t: "in", text: "echo $?" },
      { t: "out", text: "0" },
    ],
  },
  {
    id: "reactive",
    label: "reactive",
    blurb: "Idles at near-zero CPU until a server pushes a resource update.",
    title: "agentd — wake on an event",
    script: [
      { t: "in", text: "agentd --config triage.yaml" },
      { t: "out", text: '{"event":"mcp.connect","server":"queue","proto":"2026-07-28"}', cls: "comment" },
      { t: "out", text: '{"event":"mcp.subscribe","uri":"queue://inbox"}', cls: "comment" },
      { t: "out", text: '{"event":"proc.ready","workflows":1,"job_shape":false}', cls: "comment" },
      { t: "spin", text: "idle · 0 turns · waiting on queue://inbox", ms: 1600 },
      { t: "out", text: '{"event":"mcp.notify","method":"notifications/resources/updated"}', cls: "comment" },
      { t: "out", text: '{"event":"run.start","workflow":"triage","trigger":"subscribe"}', cls: "comment" },
      { t: "spin", text: "thinking · triage · 1.4k tok", ms: 900 },
      { t: "out", text: '{"event":"run.done","status":"completed","run":"triage-01M0…"}', cls: "comment" },
      { t: "spin", text: "idle · 1 turn · waiting on queue://inbox", ms: 1400 },
      { t: "out", text: "^C" },
      { t: "out", text: '{"event":"lifecycle.drain","in_flight":0}', cls: "comment" },
      { t: "out", text: '{"event":"proc.exit","code":0,"uptime_ms":18240}', cls: "comment" },
    ],
  },
  {
    id: "workflow",
    label: "workflow",
    blurb: "A graph that checkpoints before every effect — and resumes after a kill.",
    title: "agentd — crash-resume",
    script: [
      { t: "in", text: "agentd --config nightly.yaml" },
      { t: "out", text: '{"event":"workflow.loaded","name":"audit","steps":5}', cls: "comment" },
      { t: "out", text: '{"event":"run.start","run":"audit-01M0…","node":"tick"}', cls: "comment" },
      { t: "spin", text: "step: scan · cargo audit", ms: 800 },
      { t: "out", text: '{"event":"step.done","step":"scan","status":"completed"}', cls: "comment" },
      { t: "spin", text: "step: summarize · 3.1k tok", ms: 700 },
      { t: "out", text: '{"event":"checkpoint","run":"audit-01M0…","seq":4}', cls: "comment" },
      { t: "out", text: "", ms: 150 },
      { t: "out", text: "kill -9 — the host reboots", cls: "comment" },
      { t: "out", text: "", ms: 250 },
      { t: "in", text: "agentd --config nightly.yaml" },
      { t: "out", text: '{"event":"restore.run","run":"audit-01M0…","from_seq":4}', cls: "comment" },
      { t: "out", text: '{"event":"step.start","step":"report"}', cls: "comment" },
      { t: "spin", text: "step: report", ms: 600 },
      { t: "out", text: '{"event":"run.done","status":"completed","resumed":true}', cls: "comment" },
      { t: "out", text: "" },
      { t: "out", text: "The scan did not run twice." },
    ],
  },
  {
    id: "tui",
    label: "pairing",
    blurb: "A daemon you talk to. Quit the client; the agent keeps working.",
    title: "agentd tui — coder",
    script: [
      { t: "in", text: "agentd tui --config coding.yaml" },
      { t: "out", text: "agentd tui: endpoint http://127.0.0.1:8420 · logs → /tmp/agentd-tui.log", cls: "comment" },
      { t: "out", text: "" },
      { t: "out", text: "agentd · coder                    chat  tasks  subagents  debug", cls: "out" },
      { t: "out", text: "▌ why is the staging deploy flaking?", cls: "user" },
      { t: "spin", text: "thinking · 4s · 2.1k tok", ms: 1100 },
      { t: "spin", text: "exec · git log --oneline -20 · 0.3s", ms: 700 },
      { t: "spin", text: "exec · cargo test --lib · 6s", ms: 900 },
      { t: "out", text: "● Reproduced. The readiness probe races the migration job:" },
      { t: "out", text: "  api/deploy.yaml starts probing at 2s, migrations take ~9s." },
      { t: "out", text: "  Smallest fix is an initialDelaySeconds of 15." },
      { t: "out", text: "" },
      { t: "out", text: "▌ do it, but ask before you push", cls: "user" },
      { t: "spin", text: "exec · sed -i … api/deploy.yaml", ms: 700 },
      { t: "out", text: "● Patched. Ready to push to feature/probe-delay. Proceed?", cls: "warn" },
      { t: "out", text: "  ⏎ reply to continue", cls: "warn" },
      { t: "out", text: "● live  http://127.0.0.1:8420  3 turns  12.4k tok", cls: "out" },
    ],
  },
  {
    id: "bounded",
    label: "guardrails",
    blurb: "Validation before side effects; budgets that stop a runaway loop.",
    title: "agentd — the fence",
    script: [
      { t: "in", text: "agentd --validate-config -c agent.yaml" },
      { t: "out", text: '{"event":"config.invalid","msg":"workflow \\"main\\" step \\"a\\": unknown field \\"prompt\\" for kind \\"agent\\" (allowed: instruction, …)"}', cls: "err" },
      { t: "in", text: "echo $?" },
      { t: "out", text: "2" },
      { t: "out", text: "", ms: 200 },
      { t: "out", text: "# fixed, and the trifecta check has an opinion too", cls: "comment" },
      { t: "in", text: "agentd --validate-config -c agent.yaml" },
      { t: "out", text: '{"event":"config.invalid","msg":"lethal-trifecta refused: untrusted_input + sensitive + egress in one agent"}', cls: "err" },
      { t: "out", text: "", ms: 200 },
      { t: "out", text: "# narrowed the tags; now it runs — and stays inside its budget", cls: "comment" },
      { t: "in", text: "agentd -c agent.yaml" },
      { t: "spin", text: "thinking · round 12 · 198k tok", ms: 900 },
      { t: "out", text: '{"event":"budget.exhausted","window":"day","limit":200000}', cls: "comment" },
      { t: "out", text: '{"event":"run.done","status":"exhausted_tokens"}', cls: "comment" },
      { t: "in", text: "echo $?" },
      { t: "out", text: "7" },
    ],
  },
];

export default function ConsoleDemos() {
  const [active, setActive] = useState(DEMOS[0].id);
  const demo = DEMOS.find((d) => d.id === active) ?? DEMOS[0];

  return (
    <div>
      {/*
        A segmented control rather than a row of loose pills: these are five
        views of one thing, and a control that looks like one object says so.
        It scrolls sideways on a narrow screen instead of wrapping to three
        ragged rows — the terminal below it is already the tall element, and a
        stack of pills above it pushed the whole demo off the first screen.
      */}
      {/* The fade at the right edge is the only thing telling a phone reader
          there are more runs than fit; a cut-off word reads as a bug. */}
      <div className="tabscroll relative mb-3">
        <div
          className="max-w-full overflow-x-auto pb-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
          role="tablist"
          aria-label="Example runs"
        >
        <div className="inline-flex gap-1 rounded-xl border border-[var(--line)] bg-[var(--panel-2)] p-1">
          {DEMOS.map((d) => {
            const on = d.id === active;
            return (
              <button
                key={d.id}
                type="button"
                role="tab"
                aria-selected={on}
                onClick={() => setActive(d.id)}
                className={`shrink-0 whitespace-nowrap rounded-lg px-3 py-1.5 font-mono text-xs transition-colors ${
                  on
                    ? "bg-[var(--term-bg)] text-[var(--term-fg)] shadow-sm"
                    : "text-[var(--dim)] hover:bg-[var(--panel)] hover:text-[var(--fg-strong)]"
                }`}
              >
                {on && <span className="mr-1.5 text-[var(--green-solid)]">▍</span>}
                {d.label}
              </button>
            );
            })}
          </div>
        </div>
      </div>

      {/* `key` remounts the console so a newly-picked run plays from the top.
          The fixed height means that remount does not resize the page. */}
      <Console key={demo.id} title={demo.title} script={demo.script} height="21rem" />

      <p className="mt-3 text-xs text-[var(--dim)]">{demo.blurb}</p>
    </div>
  );
}
