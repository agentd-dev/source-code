"use client";

// The visual workflow editor route. React Flow touches `window`, so the editor is
// loaded client-only (no SSR) behind a dynamic import — consistent with the site's
// static export.
import dynamic from "next/dynamic";

const WorkflowEditor = dynamic(() => import("../components/WorkflowEditor"), {
  ssr: false,
  loading: () => (
    <div className="flex h-[60vh] items-center justify-center text-sm text-[var(--dim)]">
      loading the editor…
    </div>
  ),
});

export default function EditorPage() {
  return <WorkflowEditor />;
}
