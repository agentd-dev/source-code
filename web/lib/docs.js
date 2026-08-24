import fs from "node:fs";
import path from "node:path";

// The site renders the repo's authoritative markdown directly out of docs/ —
// that directory is the single source of truth and the site never forks it.
// Paths in DOCS are resolved against the repository root, one level above the
// Next.js project.
const ROOT = path.join(process.cwd(), "..");

// Ordered navigation groups. Each doc names its `group`; the sidebar, the docs
// index and the prev/next reading order all render groups in this order, so a
// group listed here with no docs simply renders nothing. A doc whose `group`
// is NOT listed here still gets a page and a sitemap entry, but is reachable
// only by its URL — keep every group a doc names in this list.
export const GROUPS = [
  { id: "start", title: "Start here" },
  { id: "concepts", title: "How it works" },
  { id: "operate", title: "Build & operate" },
  { id: "extend", title: "Extend & embed" },
  { id: "reference", title: "Reference" },
];

export const DOCS = [
  // ── Start here ────────────────────────────────────────────────
  {
    slug: "overview",
    file: "docs/README.md",
    title: "Overview",
    group: "start",
    blurb: "What agentd is, in one page — a runtime, not a framework.",
  },
  {
    slug: "getting-started",
    file: "docs/getting-started.md",
    title: "Getting started",
    group: "start",
    blurb: "Checkout to a first run; the same instruction as a job and as a daemon.",
  },
  {
    slug: "use-cases",
    file: "docs/use-cases.md",
    title: "Use cases",
    group: "start",
    blurb: "Worked shapes: jobs, reactive daemons, fan-out, trust-partition, served workers.",
  },

  // ── Concepts ──────────────────────────────────────────────────
  {
    slug: "architecture",
    file: "docs/architecture.md",
    title: "Architecture",
    group: "concepts",
    blurb: "The two-loop split, the process tree, and how a run flows from config to result.",
  },
  {
    slug: "harness",
    file: "docs/harness.md",
    title: "The harness",
    group: "concepts",
    blurb: "The supervisor that never talks to the model — process tree, kill ladder, budgets, recovery.",
  },
  {
    slug: "agent-loop",
    file: "docs/agent-loop.md",
    title: "The agent loop",
    group: "concepts",
    blurb: "One turn end to end: context assembly, the round loop, tool dispatch, termination.",
  },
  {
    slug: "modes-and-triggers",
    file: "docs/modes-and-triggers.md",
    title: "Lifecycle & triggers",
    group: "concepts",
    blurb: "Job or daemon, and the start nodes that decide when a run fires.",
  },
  {
    slug: "mcp",
    file: "docs/mcp.md",
    title: "MCP — tools & events",
    group: "concepts",
    blurb: "MCP as the substrate — the client subset, the transport, and the A2A endpoint.",
  },
  {
    slug: "a2a",
    file: "docs/a2a.md",
    title: "A2A — the inbound channel",
    group: "concepts",
    blurb: "How peers, operators and the display clients reach in: principals, tasks, and the agent card.",
  },
  {
    slug: "subagents",
    file: "docs/subagents.md",
    title: "Subagents",
    group: "concepts",
    blurb: "The same-binary re-exec subagent tree — spawn payload, scope intersection, caps.",
  },
  {
    slug: "why-rust",
    file: "docs/why-rust.md",
    title: "Why Rust",
    group: "concepts",
    blurb: "The dependency moat, what is hand-rolled, and where the choice costs something.",
  },
  {
    slug: "two-person-company",
    file: "docs/two-person-company.md",
    title: "The two-person company",
    group: "concepts",
    blurb: "An eleven-agent software company with two humans — the worked use case, and the reasoning behind every split.",
  },
  {
    slug: "pid-1",
    file: "docs/pid1.md",
    title: "PID 1 — agentd as init",
    group: "concepts",
    blurb: "A custom Linux whose init is the agent runtime — robots, appliances, and the boot story.",
  },
  {
    slug: "experience",
    file: "docs/experience.md",
    title: "Developer experience",
    group: "concepts",
    blurb: "Validate before anything runs; exit codes as an API; telemetry you can filter.",
  },
  {
    slug: "workflows",
    file: "docs/workflows.md",
    title: "Workflows",
    group: "concepts",
    blurb: "The durable DAG engine — the graph model, every node kind, durability, and worked examples.",
  },
  {
    slug: "directives",
    file: "docs/directives.md",
    title: "Directives — instruction documents",
    group: "concepts",
    blurb: "Embed workflows, skills, and context in the instruction itself (:::type{…} blocks) — one reviewable document, hot-swapped on reload, retired gracefully.",
  },
  {
    slug: "node-registry",
    file: "docs/node-registry.md",
    title: "Node registry",
    group: "reference",
    blurb: "All 67 workflow nodes with their required fields, generated from the binary's own registry — plus the five things that are easy to get wrong.",
  },

  // ── Operate ───────────────────────────────────────────────────
  {
    slug: "configuration",
    file: "docs/configuration.md",
    title: "Configuration",
    group: "operate",
    blurb: "Every key, the three spellings, precedence, and validation before anything runs.",
  },
  {
    slug: "deployment",
    file: "docs/deployment.md",
    title: "Deployment",
    group: "operate",
    blurb: "Job / CronJob / Deployment shapes, drain choreography, and the exit-code contract.",
  },
  {
    slug: "observability",
    file: "docs/observability.md",
    title: "Observability",
    group: "operate",
    blurb: "The JSON-lines event stream, metrics, OTEL traces, and the A2A read surface.",
  },
  {
    slug: "coding-agent",
    file: "docs/coding-agent.md",
    title: "Coding agent (software engineering)",
    group: "operate",
    blurb:
      "Pair-program on a repository: giving it hands (exec vs MCP), approvals, budgets, and the practices that keep it safe.",
  },
  {
    slug: "interface",
    file: "docs/interface.md",
    title: "Interface — TUI & web UI",
    group: "operate",
    blurb:
      "The display clients: one daemon, many synchronized surfaces — pairing-code login, live activity, approvals, steering.",
  },
  {
    slug: "hosting-the-ui",
    file: "docs/hosting-the-ui.md",
    title: "Hosting the web UI",
    group: "operate",
    blurb: "Running the thin client on a public domain — the daemon-side settings it needs, what a user configures, and why Safari cannot do it.",
  },
  {
    slug: "operations",
    file: "docs/operations.md",
    title: "Operations",
    group: "operate",
    blurb: "Operating a daemon over A2A — drain / lameduck / pause / resume and hot reload.",
  },
  {
    slug: "security",
    file: "docs/security.md",
    title: "Security",
    group: "operate",
    blurb: "No local execution, the Rule-of-Two, authenticated identity, and secret handling.",
  },
  {
    slug: "authentication",
    file: "docs/authentication.md",
    title: "Authentication",
    group: "operate",
    blurb: "The auth: block — static, OAuth device-login, AWS SigV4, SPIFFE — and agentd login.",
  },
  {
    slug: "scaling",
    file: "docs/scaling.md",
    title: "Scaling",
    group: "operate",
    blurb: "Many replicas over one queue: sharding, work-claim leases, and idempotency.",
  },

  // ── Extend & embed ────────────────────────────────────────────
  {
    slug: "embedding",
    file: "docs/embedding.md",
    title: "Embedding",
    group: "extend",
    blurb: "Build your own CLI on agentd-core, with native code-registered tools.",
  },
  {
    slug: "intelligence",
    file: "docs/intelligence.md",
    title: "Intelligence",
    group: "extend",
    blurb: "The single LLM wire — HTTPS transport, OpenAI-compatible, failover, and hot-swap.",
  },
  {
    slug: "aauth",
    file: "docs/aauth.md",
    title: "AAuth",
    group: "extend",
    tag: "draft",
    blurb: "Signed agent identity for AAuth-protected MCP servers.",
  },
];

export function docsInGroup(groupId) {
  return DOCS.filter((d) => d.group === groupId);
}

// Markdown basename (e.g. "configuration.md") → its slug, so a link between two
// docs can be rewritten to an on-site route. Keyed by BASENAME, not full path,
// because the source markdown links relatively ("./mcp.md", "../docs/mcp.md")
// and the basename is the only stable part. A basename the site does not host
// returns null, and the renderer falls back to a link into the repository.
const FILE_TO_SLUG = Object.fromEntries(DOCS.map((d) => [d.file.split("/").pop(), d.slug]));

export function slugForFile(name) {
  return FILE_TO_SLUG[name] ?? null;
}

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
