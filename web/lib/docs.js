import fs from "node:fs";
import path from "node:path";

// The site renders the repo's authoritative markdown directly — the docs/ and
// rfcs/ directories are the single source of truth; the site never forks them.
const ROOT = path.join(process.cwd(), "..");

// Ordered navigation groups. Each doc names its `group`; the sidebar and the
// docs index render groups in this order. RFCs are split into the 2.0 set and
// the foundations so the current specs lead.
export const GROUPS = [
  { id: "start", title: "Start here" },
  { id: "concepts", title: "Concepts" },
  { id: "operate", title: "Operate" },
  { id: "extend", title: "Extend & embed" },
  { id: "rfc-core", title: "RFCs · 2.0" },
  { id: "rfc-foundation", title: "RFCs · foundations" },
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
    slug: "modes-and-triggers",
    file: "docs/modes-and-triggers.md",
    title: "Lifecycle & triggers",
    group: "concepts",
    blurb: "The 2.0 lifecycle (job vs daemon) and the workflow start-node triggers.",
  },
  {
    slug: "mcp",
    file: "docs/mcp.md",
    title: "MCP & A2A surface",
    group: "concepts",
    blurb: "MCP as the substrate — the client subset, the transport, and the A2A endpoint.",
  },
  {
    slug: "subagents",
    file: "docs/subagents.md",
    title: "Subagents",
    group: "concepts",
    blurb: "The same-binary re-exec subagent tree — spawn payload, scope intersection, caps.",
  },
  {
    slug: "workflows",
    file: "docs/workflows.md",
    title: "Workflows",
    group: "concepts",
    blurb: "The durable DAG engine (RFC 0027) — the graph model, every node kind, durability, and worked examples.",
  },

  // ── Operate ───────────────────────────────────────────────────
  {
    slug: "configuration",
    file: "docs/configuration.md",
    title: "Configuration",
    group: "operate",
    blurb: "Every key and flag, precedence, validate-at-startup, and a complete 2.0 config.",
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
    slug: "interface",
    file: "docs/interface.md",
    title: "Interface — TUI & web UI",
    group: "operate",
    blurb:
      "The display clients: one daemon, many synchronized surfaces — pairing-code login, live activity, approvals, steering.",
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
    tag: "1.x",
    blurb: "Scale with replicas over a shared durable store; the 1.x cluster page is retained.",
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

  // ── RFCs · 2.0 ────────────────────────────────────────────────
  { slug: "rfc-0026", file: "rfcs/0026-agent-loop-and-lifecycle.md", title: "0026 · Agent loop & lifecycle", group: "rfc-core" },
  { slug: "rfc-0027", file: "rfcs/0027-workflow-dialect-3.md", title: "0027 · Workflow dialect v3", group: "rfc-core" },
  { slug: "rfc-0029", file: "rfcs/0029-a2a-conversations-principals-commands.md", title: "0029 · A2A conversations & principals", group: "rfc-core" },
  { slug: "rfc-0025", file: "rfcs/0025-durable-state-and-store-adapters.md", title: "0025 · Durable state & store", group: "rfc-core" },
  { slug: "rfc-0028", file: "rfcs/0028-tools-registry-and-internal-tools.md", title: "0028 · Tools registry", group: "rfc-core" },
  { slug: "rfc-0030", file: "rfcs/0030-config-schema-v2.md", title: "0030 · Config schema v2", group: "rfc-core" },
  { slug: "rfc-0031", file: "rfcs/0031-endpoint-authentication.md", title: "0031 · Endpoint authentication", group: "rfc-core" },
  { slug: "rfc-0032", file: "rfcs/0032-interface-and-observation-plane.md", title: "0032 · Interface & observation plane", group: "rfc-core" },
  { slug: "rfc-0022", file: "rfcs/0022-embedding-and-code-tools.md", title: "0022 · Embedding & code tools", group: "rfc-core" },
  { slug: "rfc-0023", file: "rfcs/0023-aauth-agent-identity.md", title: "0023 · AAuth agent identity", group: "rfc-core", tag: "draft" },
  { slug: "rfc-0024", file: "rfcs/0024-evaluation-harness.md", title: "0024 · Evaluation harness", group: "rfc-core" },

  // ── RFCs · foundations ────────────────────────────────────────
  { slug: "rfc-0001", file: "rfcs/0001-mcp-native-agent-runtime.md", title: "0001 · MCP-native runtime", group: "rfc-foundation" },
  { slug: "rfc-0002", file: "rfcs/0002-supervisor-reactor-and-concurrency.md", title: "0002 · Supervisor & concurrency", group: "rfc-foundation" },
  { slug: "rfc-0003", file: "rfcs/0003-process-supervision-and-recovery.md", title: "0003 · Supervision & recovery", group: "rfc-foundation" },
  { slug: "rfc-0004", file: "rfcs/0004-mcp-client-subset-and-codec.md", title: "0004 · MCP client & codec", group: "rfc-foundation" },
  { slug: "rfc-0006", file: "rfcs/0006-intelligence-transport-and-wire.md", title: "0006 · Intelligence transport", group: "rfc-foundation" },
  { slug: "rfc-0007", file: "rfcs/0007-agentic-loop-and-terminal-status.md", title: "0007 · Agentic loop", group: "rfc-foundation" },
  { slug: "rfc-0009", file: "rfcs/0009-subagent-process-model.md", title: "0009 · Subagent model", group: "rfc-foundation" },
  { slug: "rfc-0010", file: "rfcs/0010-observability-health-telemetry.md", title: "0010 · Observability", group: "rfc-foundation" },
  { slug: "rfc-0011", file: "rfcs/0011-cloud-native-contract.md", title: "0011 · Cloud-native contract", group: "rfc-foundation" },
  { slug: "rfc-0012", file: "rfcs/0012-security-posture.md", title: "0012 · Security posture", group: "rfc-foundation" },
  { slug: "rfc-0014", file: "rfcs/0014-control-plane-contract.md", title: "0014 · Control-plane contract", group: "rfc-foundation" },
  { slug: "rfc-0015", file: "rfcs/0015-management-and-control-surface.md", title: "0015 · Management surface", group: "rfc-foundation" },
  { slug: "rfc-0016", file: "rfcs/0016-telemetry-and-lifecycle-contract.md", title: "0016 · Telemetry contract", group: "rfc-foundation" },
  { slug: "rfc-0017", file: "rfcs/0017-declarative-config-and-hot-reload.md", title: "0017 · Config & hot reload", group: "rfc-foundation" },
  { slug: "rfc-0018", file: "rfcs/0018-intelligence-transport-resilience.md", title: "0018 · Intelligence resilience", group: "rfc-foundation" },
  { slug: "rfc-0005", file: "rfcs/0005-self-mcp-server-and-control-protocol.md", title: "0005 · Self-MCP server", group: "rfc-foundation", tag: "1.x" },
  { slug: "rfc-0008", file: "rfcs/0008-execution-modes-and-reactive-routing.md", title: "0008 · Modes & reactivity", group: "rfc-foundation", tag: "1.x" },
  { slug: "rfc-0013", file: "rfcs/0013-deferred-v2-surface.md", title: "0013 · Deferred v2 surface", group: "rfc-foundation" },
  { slug: "rfc-0019", file: "rfcs/0019-horizontal-scaling.md", title: "0019 · Horizontal scaling", group: "rfc-foundation", tag: "1.x" },
  { slug: "rfc-0020", file: "rfcs/0020-a2a-interop-over-vsock.md", title: "0020 · A2A over vsock", group: "rfc-foundation", tag: "1.x" },
  { slug: "rfc-0021", file: "rfcs/0021-durable-workflows-and-parity-extensions.md", title: "0021 · Durable workflows", group: "rfc-foundation", tag: "1.x" },
];

export function docsInGroup(groupId) {
  return DOCS.filter((d) => d.group === groupId);
}

// docs file path (e.g. "configuration.md" or "rfcs/0011-….md") → its slug, so
// inter-doc markdown links can be rewritten to on-site routes.
const FILE_TO_SLUG = Object.fromEntries(DOCS.map((d) => [d.file.split("/").pop(), d.slug]));

export function slugForFile(name) {
  return FILE_TO_SLUG[name] ?? null;
}

export function readDoc(slug) {
  const entry = DOCS.find((d) => d.slug === slug);
  if (!entry) return null;
  const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
  return { ...entry, raw };
}
