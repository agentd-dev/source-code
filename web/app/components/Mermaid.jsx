"use client";

import { useEffect, useRef, useState } from "react";

/* mermaid is ~1MB — load it lazily, once, only on pages that actually render a
   diagram, and theme it to the site tokens so diagrams read as part of the page
   rather than a bolted-on widget. */
let mermaidPromise = null;
function getMermaid() {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then(({ default: mermaid }) => mermaid);
  }
  return mermaidPromise;
}

const FONT =
  '"JetBrains Mono", ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace';

/**
 * The two palettes, one per theme.
 *
 * The light one is not the dark one lightened: a diagram inherits the page it
 * sits on, and near-black boxes with white text on a white page read as a
 * screenshot someone pasted in. Every pair here was measured — text at or above
 * 4.5:1, lines and borders at or above 3:1 (WCAG 1.4.3 and 1.4.11) — against
 * `--panel`, which is what `.mermaid-figure` paints behind them.
 *
 * The border values are deliberate on both sides: each is the *lightest* (dark
 * theme: the darkest) value that still clears 3:1 against both the node fill and
 * the page behind it, so boxes are defined without the diagram becoming a grid
 * of hard outlines. The dark theme used to fail that badly — `#2a2a30` on a
 * `#141417` fill is 1.3:1, a border you cannot actually see.
 */
const PALETTE = {
  light: {
    darkMode: false,
    primaryColor: "#eef0f3",
    primaryBorderColor: "#82828c",
    primaryTextColor: "#18181b",
    secondaryColor: "#f4f4f6",
    secondaryBorderColor: "#82828c",
    secondaryTextColor: "#27272a",
    tertiaryColor: "#f8f8fa",
    tertiaryBorderColor: "#a1a1aa",
    tertiaryTextColor: "#27272a",
    mainBkg: "#eef0f3",
    nodeBorder: "#82828c",
    nodeTextColor: "#18181b",
    lineColor: "#5f5f68",
    textColor: "#27272a",
    titleColor: "#18181b",
    clusterBkg: "#f4faf6",
    clusterBorder: "#82828c",
    edgeLabelBackground: "#ffffff",
    labelBoxBkgColor: "#eef0f3",
    labelBoxBorderColor: "#82828c",
    labelTextColor: "#18181b",
    actorBkg: "#eef0f3",
    actorBorder: "#82828c",
    actorTextColor: "#18181b",
    actorLineColor: "#82828c",
    signalColor: "#3f3f46",
    signalTextColor: "#18181b",
    sequenceNumberColor: "#ffffff",
    noteBkgColor: "#e8f6ed",
    noteBorderColor: "#15803d",
    noteTextColor: "#14532d",
    altBackground: "#f4f4f6",
  },
  dark: {
    darkMode: true,
    primaryColor: "#141417",
    primaryBorderColor: "#63636f",
    primaryTextColor: "#f4f4f5",
    secondaryColor: "#101012",
    secondaryBorderColor: "#63636f",
    secondaryTextColor: "#d4d4d8",
    tertiaryColor: "#0e0e10",
    tertiaryBorderColor: "#6b6b78",
    tertiaryTextColor: "#d4d4d8",
    mainBkg: "#141417",
    nodeBorder: "#63636f",
    nodeTextColor: "#f4f4f5",
    lineColor: "#8b8b94",
    textColor: "#d4d4d8",
    titleColor: "#f4f4f5",
    clusterBkg: "rgba(74,222,128,0.04)",
    clusterBorder: "#63636f",
    edgeLabelBackground: "#141417",
    labelBoxBkgColor: "#141417",
    labelBoxBorderColor: "#63636f",
    labelTextColor: "#f4f4f5",
    actorBkg: "#141417",
    actorBorder: "#63636f",
    actorTextColor: "#f4f4f5",
    actorLineColor: "#63636f",
    signalColor: "#a1a1aa",
    signalTextColor: "#d4d4d8",
    sequenceNumberColor: "#0c0c0e",
    noteBkgColor: "#0e1a12",
    noteBorderColor: "#22c55e",
    noteTextColor: "#d4d4d8",
    altBackground: "#101012",
  },
};

/** Which palette the page is actually showing right now. */
function currentTheme() {
  if (typeof document === "undefined") return "dark";
  const explicit = document.documentElement.getAttribute("data-theme");
  if (explicit === "light" || explicit === "dark") return explicit;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function configure(mermaid, theme) {
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "base",
    fontFamily: FONT,
    themeVariables: {
      background: "transparent",
      fontSize: "13px",
      ...PALETTE[theme],
    },
    flowchart: { curve: "basis", htmlLabels: true, padding: 12 },
    sequence: { useMaxWidth: true, mirrorActors: false },
  });
}

let counter = 0;

export default function Mermaid({ chart }) {
  const ref = useRef(null);
  const [failed, setFailed] = useState(false);
  // Re-renders the SVG when the reader changes theme. A diagram is baked at
  // render time, so without this a light/dark switch leaves the old palette
  // sitting on the new page — the one place on the site that would not follow
  // the toggle.
  const [theme, setTheme] = useState(currentTheme);

  useEffect(() => {
    const sync = () => setTheme(currentTheme());
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    media.addEventListener("change", sync);
    // The toggle stamps (or clears) `data-theme` on the root element.
    const observer = new MutationObserver(sync);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    sync();
    return () => {
      media.removeEventListener("change", sync);
      observer.disconnect();
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    setFailed(false);
    getMermaid()
      .then((mermaid) => {
        configure(mermaid, theme);
        return mermaid.render(`mmd-${counter++}`, chart);
      })
      .then(({ svg }) => {
        if (!cancelled && ref.current) ref.current.innerHTML = svg;
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, [chart, theme]);

  // Graceful fallback: if a diagram can't render (or JS is off), show the source.
  if (failed) {
    return (
      <pre className="mermaid-fallback" aria-label="diagram source">
        {chart}
      </pre>
    );
  }

  return (
    <figure className="mermaid-figure" role="img" aria-label="diagram">
      <div ref={ref} className="mermaid-diagram" />
    </figure>
  );
}
