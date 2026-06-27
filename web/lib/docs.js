import fs from "node:fs";
import path from "node:path";

// The site renders the repo's authoritative markdown directly — the docs/ and
// rfcs/ directories are the single source of truth; the site never forks them.
const ROOT = path.join(process.cwd(), "..");

export const DOCS = [
  { slug: "overview", file: "docs/README.md", title: "overview" },
  { slug: "getting-started", file: "docs/getting-started.md", title: "getting started" },
  { slug: "use-cases", file: "docs/use-cases.md", title: "use cases" },
  { slug: "architecture", file: "docs/architecture.md", title: "architecture" },
  { slug: "mcp", file: "docs/mcp.md", title: "mcp surface" },
  { slug: "modes-and-triggers", file: "docs/modes-and-triggers.md", title: "modes & triggers" },
  { slug: "subagents", file: "docs/subagents.md", title: "subagents" },
  { slug: "intelligence", file: "docs/intelligence.md", title: "intelligence" },
  { slug: "configuration", file: "docs/configuration.md", title: "configuration" },
  { slug: "observability", file: "docs/observability.md", title: "observability" },
  { slug: "security", file: "docs/security.md", title: "security" },
  { slug: "deployment", file: "docs/deployment.md", title: "deployment" },
  { slug: "rfc-0001", file: "rfcs/0001-mcp-native-agent-runtime.md", title: "rfc 0001 · runtime" },
  { slug: "rfc-0002", file: "rfcs/0002-supervisor-reactor-and-concurrency.md", title: "rfc 0002 · supervisor & concurrency" },
  { slug: "rfc-0003", file: "rfcs/0003-process-supervision-and-recovery.md", title: "rfc 0003 · supervision & recovery" },
  { slug: "rfc-0004", file: "rfcs/0004-mcp-client-subset-and-codec.md", title: "rfc 0004 · mcp client & codec" },
  { slug: "rfc-0005", file: "rfcs/0005-self-mcp-server-and-control-protocol.md", title: "rfc 0005 · self-mcp server" },
  { slug: "rfc-0007", file: "rfcs/0007-agentic-loop-and-terminal-status.md", title: "rfc 0007 · agentic loop" },
  { slug: "rfc-0008", file: "rfcs/0008-execution-modes-and-reactive-routing.md", title: "rfc 0008 · modes & reactivity" },
  { slug: "rfc-0009", file: "rfcs/0009-subagent-process-model.md", title: "rfc 0009 · subagent model" },
  { slug: "rfc-0011", file: "rfcs/0011-cloud-native-contract.md", title: "rfc 0011 · cloud-native contract" },
  { slug: "rfc-0012", file: "rfcs/0012-security-posture.md", title: "rfc 0012 · security posture" },
];

// docs file path (e.g. "configuration.md" or "rfcs/0011-….md") → its slug, so
// inter-doc markdown links can be rewritten to on-site routes.
const FILE_TO_SLUG = Object.fromEntries(
  DOCS.map((d) => [d.file.split("/").pop(), d.slug])
);

export function slugForFile(name) {
  return FILE_TO_SLUG[name] ?? null;
}

export function readDoc(slug) {
  const entry = DOCS.find((d) => d.slug === slug);
  if (!entry) return null;
  const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
  return { ...entry, raw };
}
