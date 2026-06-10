import fs from "node:fs";
import path from "node:path";

// The site renders the repo's authoritative markdown directly — the
// docs/ directory is the single source of truth; the site never
// forks it.
const ROOT = path.join(process.cwd(), "..");

export const DOCS = [
  { slug: "overview", file: "docs/README.md", title: "overview" },
  { slug: "architecture", file: "docs/architecture.md", title: "architecture" },
  { slug: "capabilities", file: "docs/capabilities.md", title: "capabilities" },
  { slug: "configuration", file: "docs/configuration.md", title: "configuration" },
  { slug: "operations", file: "docs/operations.md", title: "operations" },
  { slug: "maturity", file: "docs/maturity.md", title: "maturity" },
  {
    slug: "rfc-0001",
    file: "rfcs/0001-bounded-workflow-runtime.md",
    title: "rfc 0001 · bounded workflow runtime",
  },
  {
    slug: "rfc-0002",
    file: "rfcs/0002-signed-workflows.md",
    title: "rfc 0002 · signed workflows",
  },
  {
    slug: "rfc-0003",
    file: "rfcs/0003-execution-model.md",
    title: "rfc 0003 · the agent loop",
  },
  {
    slug: "rfc-0004",
    file: "rfcs/0004-multi-server-mcp.md",
    title: "rfc 0004 · multi-server mcp",
  },
  {
    slug: "rfc-0005",
    file: "rfcs/0005-hot-reload.md",
    title: "rfc 0005 · hot reload",
  },
  {
    slug: "rfc-0006",
    file: "rfcs/0006-dynamic-harness.md",
    title: "rfc 0006 · the dynamic harness",
  },
  { slug: "roadmap", file: "docs/ROADMAP.md", title: "roadmap" },
];

export function readDoc(slug) {
  const entry = DOCS.find((d) => d.slug === slug);
  if (!entry) return null;
  const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
  return { ...entry, raw };
}
