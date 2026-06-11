"use client";

// Mobile-only nav dropdown. On small screens the header would crowd
// five links into ~360px; group the secondary surfaces (rfcs, use
// cases, inspect) under one "resources" menu and keep docs + github
// directly tappable. Desktop (md+) renders the flat links instead —
// this component hides itself.
//
// A client component because the App Router layout persists across
// client-side navigations: the menu must close itself on link click
// and on outside taps.

import Link from "next/link";
import { useEffect, useRef, useState } from "react";

const ITEMS = [
  { href: "/docs/rfc-0001/", label: "rfcs" },
  { href: "/use-cases/", label: "use cases" },
  { href: "/inspect/", label: "inspect" },
];

export default function ResourcesMenu() {
  const [open, setOpen] = useState(false);
  const root = useRef(null);

  useEffect(() => {
    if (!open) return;
    const onPointer = (e) => {
      if (root.current && !root.current.contains(e.target)) setOpen(false);
    };
    const onKey = (e) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div ref={root} className="relative md:hidden">
      <button
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="text-[var(--dim)] hover:text-[var(--accent)]"
      >
        resources <span aria-hidden="true">{open ? "▴" : "▾"}</span>
      </button>
      {open && (
        <div
          role="menu"
          className="frame absolute left-0 top-full z-50 mt-2 min-w-36 py-1"
        >
          {ITEMS.map((item) => (
            <Link
              key={item.href}
              role="menuitem"
              href={item.href}
              onClick={() => setOpen(false)}
              className="block px-3 py-1.5 text-[var(--dim)] hover:text-[var(--accent)]"
            >
              {item.label}
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}
