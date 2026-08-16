"use client";

import { useEffect, useRef, useState } from "react";

/* mermaid is ~1MB — load it lazily, once, only on pages that actually render a
   diagram, and theme it to the site tokens so diagrams read as part of the page
   rather than a bolted-on widget. */
let mermaidPromise = null;
function getMermaid() {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then(({ default: mermaid }) => {
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: "base",
        fontFamily:
          '"JetBrains Mono", ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, monospace',
        themeVariables: {
          darkMode: true,
          background: "transparent",
          fontSize: "13px",
          primaryColor: "#141417",
          primaryBorderColor: "#2a2a30",
          primaryTextColor: "#f4f4f5",
          secondaryColor: "#101012",
          secondaryBorderColor: "#1f1f23",
          secondaryTextColor: "#d4d4d8",
          tertiaryColor: "#0e0e10",
          tertiaryBorderColor: "#1f1f23",
          tertiaryTextColor: "#d4d4d8",
          mainBkg: "#141417",
          nodeBorder: "#2a2a30",
          nodeTextColor: "#f4f4f5",
          lineColor: "#6b6b73",
          textColor: "#d4d4d8",
          clusterBkg: "rgba(74,222,128,0.03)",
          clusterBorder: "#1f1f23",
          edgeLabelBackground: "#0c0c0e",
          labelBoxBkgColor: "#141417",
          labelBoxBorderColor: "#2a2a30",
          // sequence / state accents
          actorBkg: "#141417",
          actorBorder: "#2a2a30",
          actorTextColor: "#f4f4f5",
          signalColor: "#8b8b94",
          signalTextColor: "#d4d4d8",
          noteBkgColor: "#0e1a12",
          noteBorderColor: "#22c55e",
          noteTextColor: "#d4d4d8",
        },
        flowchart: { curve: "basis", htmlLabels: true, padding: 12 },
        sequence: { useMaxWidth: true, mirrorActors: false },
      });
      return mermaid;
    });
  }
  return mermaidPromise;
}

let counter = 0;

export default function Mermaid({ chart }) {
  const ref = useRef(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setFailed(false);
    getMermaid()
      .then((mermaid) => mermaid.render(`mmd-${counter++}`, chart))
      .then(({ svg }) => {
        if (!cancelled && ref.current) ref.current.innerHTML = svg;
      })
      .catch(() => {
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, [chart]);

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
