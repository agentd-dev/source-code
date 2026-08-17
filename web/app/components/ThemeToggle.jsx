"use client";

import { useEffect, useState } from "react";

/**
 * Light / dark / system, in that cycle.
 *
 * The stored value is the user's *intent* ("system" is a real choice, not the
 * absence of one), so `data-theme` is only stamped for an explicit light/dark
 * pick — leaving `prefers-color-scheme` to decide otherwise. The matching
 * no-flash script in the layout applies the same rule before first paint.
 */
const ORDER = ["system", "light", "dark"];

const ICON = {
  system: (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.7">
      <rect x="2.5" y="4" width="19" height="13" rx="2" />
      <path d="M8.5 20.5h7" />
    </svg>
  ),
  light: (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.7">
      <circle cx="12" cy="12" r="4.2" />
      <path d="M12 2v2.2M12 19.8V22M2 12h2.2M19.8 12H22M4.9 4.9l1.6 1.6M17.5 17.5l1.6 1.6M19.1 4.9l-1.6 1.6M6.5 17.5l-1.6 1.6" />
    </svg>
  ),
  dark: (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.7">
      <path d="M20 13.5A8.2 8.2 0 1 1 10.5 4a6.6 6.6 0 0 0 9.5 9.5z" />
    </svg>
  ),
};

export default function ThemeToggle() {
  const [mode, setMode] = useState("system");

  // Read the stored intent after mount: the server-rendered markup must not
  // depend on it (there is no such thing as a themed static export).
  useEffect(() => {
    const saved = localStorage.getItem("theme");
    if (saved === "light" || saved === "dark" || saved === "system") setMode(saved);
  }, []);

  function apply(next) {
    setMode(next);
    localStorage.setItem("theme", next);
    const root = document.documentElement;
    if (next === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", next);
  }

  return (
    <button
      type="button"
      className="icon-btn"
      onClick={() => apply(ORDER[(ORDER.indexOf(mode) + 1) % ORDER.length])}
      aria-label={`Theme: ${mode}. Click to change.`}
      title={`Theme: ${mode}`}
    >
      {ICON[mode]}
    </button>
  );
}
