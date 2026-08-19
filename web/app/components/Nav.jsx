"use client";

import Link from "next/link";
import { useEffect, useRef } from "react";
import ThemeToggle from "./ThemeToggle";

/**
 * The header.
 *
 * Previously it was a flat row of eight links — four of them anchors into one
 * page — which made the site look like a single document with a footer. It is
 * now two menus and two destinations: everything a reader might want is one
 * hover away, and the bar itself stays quiet.
 *
 * The menus are `<details>` elements, so they work without JavaScript; the
 * only script here closes an open menu on outside-click or Escape, which is
 * the part the platform does not give us.
 */
const LEARN = [
  { href: "/docs/overview/", t: "Overview", d: "What agentd is, and the shape of a run" },
  { href: "/docs/getting-started/", t: "Getting started", d: "Install, configure, first run" },
  { href: "/docs/coding-agent/", t: "Build a coding agent", d: "A pair-programming daemon you host" },
  { href: "/docs/use-cases/", t: "Use cases", d: "The shapes people actually deploy" },
];

const UNDERSTAND = [
  { href: "/docs/architecture/", t: "Architecture", d: "Two loops, one binary, strict separation" },
  { href: "/docs/harness/", t: "The harness", d: "The supervisor that never talks to the model" },
  { href: "/docs/agent-loop/", t: "The agent loop", d: "How a turn is built, run, and ended" },
  { href: "/docs/workflows/", t: "Workflows", d: "Durable DAGs, triggers, and resume" },
  { href: "/docs/subagents/", t: "Subagents", d: "Delegation as an OS process tree" },
  { href: "/docs/why-rust/", t: "Why Rust", d: "The dependency moat, and what it buys" },
];

const REFERENCE = [
  { href: "/docs/node-registry/", t: "Node registry", d: "All 67 workflow nodes, and the five traps" },
  { href: "/docs/configuration/", t: "Configuration", d: "Every key, and how layers resolve" },
  { href: "/docs/interface/", t: "TUI & web UI", d: "The display clients and their protocol" },
  { href: "/docs/mcp/", t: "MCP", d: "Where tools and events come from" },
  { href: "/docs/security/", t: "Security", d: "Capability scoping and the trifecta rule" },
  { href: "/docs/observability/", t: "Observability", d: "Logs, metrics, traces" },
  { href: "/docs/", t: "All documentation →", d: null },
];

function Menu({ label, items }) {
  const ref = useRef(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onDocClick = (e) => {
      if (el.open && !el.contains(e.target)) el.open = false;
    };
    const onKey = (e) => {
      if (e.key === "Escape" && el.open) el.open = false;
    };
    document.addEventListener("click", onDocClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("click", onDocClick);
      document.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <details className="menu" ref={ref}>
      <summary>{label}</summary>
      <div className="menu-panel">
        {items.map((it) =>
          it.d === null ? (
            <div key={it.href}>
              <div className="menu-sep" />
              <Link className="menu-item" href={it.href} onClick={() => (ref.current.open = false)}>
                <div className="t">{it.t}</div>
              </Link>
            </div>
          ) : (
            <Link
              key={it.href}
              className="menu-item"
              href={it.href}
              onClick={() => (ref.current.open = false)}
            >
              <div className="t">{it.t}</div>
              <div className="d">{it.d}</div>
            </Link>
          ),
        )}
      </div>
    </details>
  );
}

export default function Nav() {
  return (
    <header className="site-header">
      <nav aria-label="primary" className="inner">
        <Link href="/" className="brand">
          agentd<span className="at">@</span>
          <span className="tilde">~</span>
        </Link>

        <div className="hidden items-center gap-0.5 md:flex">
          <Menu label="Learn" items={LEARN} />
          <Menu label="Concepts" items={UNDERSTAND} />
          <Menu label="Reference" items={REFERENCE} />
          <Link className="nav-link" href="/editor/">
            Editor
          </Link>
        </div>

        <div className="ml-auto flex items-center gap-1">
          <Link className="nav-link md:hidden" href="/docs/">
            Docs
          </Link>
          <ThemeToggle />
          <a
            className="icon-btn"
            href="https://github.com/agentd-dev/source-code"
            aria-label="GitHub"
            title="GitHub"
          >
            <svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor">
              <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.4 7.4 0 0 1 2-.27c.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
            </svg>
          </a>
        </div>
      </nav>
    </header>
  );
}
