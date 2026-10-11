"use client";

import Link from "next/link";
import { useEffect, useRef } from "react";
import { DOCS, GROUPS } from "../../lib/docs-registry";
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
/* The menus come from the docs registry: one per group that names a `menu`
   label, listing the group's `featured` pages (with their short `hint`), and
   the docs index last. A hand-kept copy here drifted from the pages it named. */
const MENUS = GROUPS.filter((g) => g.menu).map((g) => ({
  label: g.menu,
  items: [
    ...DOCS.filter((d) => d.group === g.id && d.featured).map((d) => ({
      href: `/docs/${d.slug}/`,
      t: d.title,
      d: d.hint ?? d.blurb,
    })),
    { href: "/docs/", t: "All documentation →", d: null },
  ],
}));

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
          {MENUS.map((m) => (
            <Menu key={m.label} label={m.label} items={m.items} />
          ))}
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
