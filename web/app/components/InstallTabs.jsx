"use client";

import { useState } from "react";

/*
  The hero's "get it" terminal. One panel, four ways in — a segmented control
  (the same object the console demos use, so the hero reads as one design
  system) picks the channel, and the copy button in the title bar copies ONLY
  the command for that channel, never the output lines around it.

  The four channels are deliberately different audiences:
    curl    — a person at a shell (the canonical path; checksum-verified).
    docker  — an operator who deploys images, not binaries.
    npm     — the display clients (TUI + web) for a daemon running elsewhere.
    agent   — not a command at all: a PROMPT someone pastes to their AI agent,
              pointing it at install.sh, llms.txt and the Agent Skill so the
              agent can do the setup itself.
*/
const CHANNELS = [
  {
    id: "curl",
    label: "curl",
    title: "install · linux amd64/arm64 · checksum-verified",
    copy: "curl -fsSL https://agentd.dev/install.sh | sh",
    display: `$ curl -fsSL https://agentd.dev/install.sh | sh
agentd  checksum ok
agentd  installed agentd to /usr/local/bin/agentd`,
  },
  {
    id: "docker",
    label: "docker",
    title: "container · ghcr.io · signed, SBOM-attested",
    copy: "docker run --rm -v $PWD:/etc/agentd:ro ghcr.io/agentd-dev/agentd:latest -c /etc/agentd/agent.yaml",
    display: `$ docker run --rm -v $PWD:/etc/agentd:ro \\
    ghcr.io/agentd-dev/agentd:latest -c /etc/agentd/agent.yaml
{"event":"proc.start","instance":"agent","version":"2.5.1"}`,
  },
  {
    id: "npm",
    label: "npm",
    title: "display clients · TUI + web UI · attach to a daemon",
    copy: "npm install -g @agentd-dev/cli",
    display: `$ npm install -g @agentd-dev/cli
$ agentd-tui --endpoint http://127.0.0.1:8420
● attached — same live state as every other client`,
  },
  {
    id: "agent",
    label: "ai agent",
    title: "for your AI agent · paste as a prompt",
    copy: `Install agentd on this machine: run \`curl -fsSL https://agentd.dev/install.sh | sh\` (it verifies checksums, no sudo). Then read https://agentd.dev/llms.txt and the agentd Agent Skill at https://github.com/agentd-dev/source-code/tree/main/skills/agentd, and set up an agentd config for my task.`,
    display: `Install agentd on this machine: run
  curl -fsSL https://agentd.dev/install.sh | sh
(it verifies checksums, no sudo). Then read
  https://agentd.dev/llms.txt
and the agentd Agent Skill at
  github.com/agentd-dev/source-code · skills/agentd
and set up an agentd config for my task.`,
  },
];

export default function InstallTabs() {
  const [active, setActive] = useState(CHANNELS[0].id);
  const [copied, setCopied] = useState(false);
  const ch = CHANNELS.find((c) => c.id === active) ?? CHANNELS[0];

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(ch.copy);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* clipboard blocked (http, permissions) — select-and-copy still works */
    }
  };

  return (
    <div>
      <div className="tabscroll relative mb-3">
        <div
          className="max-w-full overflow-x-auto pb-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
          role="tablist"
          aria-label="Install channels"
        >
          <div className="inline-flex gap-1 rounded-xl border border-[var(--line)] bg-[var(--panel-2)] p-1">
            {CHANNELS.map((c) => {
              const on = c.id === active;
              return (
                <button
                  key={c.id}
                  type="button"
                  role="tab"
                  aria-selected={on}
                  onClick={() => {
                    setActive(c.id);
                    setCopied(false);
                  }}
                  className={`shrink-0 whitespace-nowrap rounded-lg px-3 py-1.5 font-mono text-xs transition-colors ${
                    on
                      ? "bg-[var(--term-bg)] text-[var(--term-fg)] shadow-sm"
                      : "text-[var(--dim)] hover:bg-[var(--panel)] hover:text-[var(--fg-strong)]"
                  }`}
                >
                  {on && <span className="mr-1.5 text-[var(--green-solid)]">▍</span>}
                  {c.label}
                </button>
              );
            })}
          </div>
        </div>
      </div>

      <div className="term">
        <div className="panel-title">
          <span className="dots">
            <i />
            <i />
            <i />
          </span>
          <span className="ml-1 min-w-0 truncate">{ch.title}</span>
          <button
            type="button"
            onClick={copy}
            aria-label={`Copy the ${ch.label} command`}
            className={`ml-auto shrink-0 rounded-md border border-[var(--line)] px-2 py-0.5 font-mono text-[11px] transition-colors ${
              copied
                ? "text-[var(--green-solid)]"
                : "text-[var(--dim)] hover:border-[var(--fg-strong)] hover:text-[var(--fg-strong)]"
            }`}
          >
            {copied ? "copied ✓" : "copy"}
          </button>
        </div>
        {/* A fixed height so switching channels never reflows the hero. */}
        <pre className="min-h-[7.5rem]">{ch.display}</pre>
      </div>
    </div>
  );
}
