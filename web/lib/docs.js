import fs from "node:fs";
import path from "node:path";
import { DOCS } from "./docs-registry";

export * from "./docs-registry";

// The site renders the repo's authoritative markdown directly out of docs/ —
// that directory is the single source of truth and the site never forks it.
// Paths in DOCS are resolved against the repository root, one level above the
// Next.js project.
const ROOT = path.join(process.cwd(), "..");

export function readDoc(slug) {
  const entry = DOCS.find((d) => d.slug === slug);
  if (!entry) return null;
  // A registry entry whose file is missing must not take the whole build down
  // with it — the page renders as not-found and every other doc still ships.
  try {
    const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
    return { ...entry, raw };
  } catch {
    return null;
  }
}
