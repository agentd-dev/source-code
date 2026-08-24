export const dynamic = "force-static";

// /sitemap.xml — generated at build from the docs registry, so a page added
// to lib/docs.js is in the sitemap by construction. lastModified comes from
// the source markdown's mtime (the git checkout the site builds from).
import { statSync } from "node:fs";
import { join } from "node:path";
import { DOCS } from "../lib/docs";

const BASE = "https://agentd.dev";
const REPO = join(process.cwd(), "..");

function mtime(relPath) {
  try {
    return statSync(join(REPO, relPath)).mtime;
  } catch {
    return new Date();
  }
}

export default function sitemap() {
  const now = new Date();
  const top = [
    { url: `${BASE}/`, lastModified: now, changeFrequency: "weekly", priority: 1.0 },
    { url: `${BASE}/docs/`, lastModified: now, changeFrequency: "weekly", priority: 0.9 },
    { url: `${BASE}/editor/`, lastModified: now, changeFrequency: "monthly", priority: 0.5 },
  ];
  const docs = DOCS.map((d) => ({
    url: `${BASE}/docs/${d.slug}/`,
    lastModified: mtime(d.file),
    changeFrequency: "weekly",
    priority: 0.8,
  }));
  return [...top, ...docs];
}
