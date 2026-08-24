import fs from "node:fs";
import path from "node:path";

// The site renders the repo's authoritative markdown directly — the docs/ and
// rfcs/ directories are the single source of truth; the site never forks them.
const ROOT = path.join(process.cwd(), "..");

// Ordered navigation groups. Each doc names its `group`; the sidebar and the
// docs index render groups in this order. Specs are split so the ones that
// describe the current design lead, with the foundations behind them.
export const GROUPS = [
  { id: "start", title: "Start here" },
  { id: "concepts", title: "How it works" },
  { id: "operate", title: "Build & operate" },
  { id: "extend", title: "Extend & embed" },
  { id: "rfc-core", title: "Specifications" },
  { id: "rfc-foundation", title: "Specifications · foundations" },
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
    blurb: "The durable DAG engine (RFC 0027) — the graph model, every node kind, durability, and worked examples.",
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
    blurb: "Running the thin client on a public domain — what agentd had to change, what a user configures, and why Safari cannot do it.",
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

  // ── the specs behind the current design ───────────────────────
  { slug: "rfc-0026", file: "rfcs/0026-agent-loop-and-lifecycle.md", title: "0026 · Agent loop & lifecycle", group: "rfc-core" },
  { slug: "rfc-0027", file: "rfcs/0027-workflow-dialect-3.md", title: "0027 · Workflow dialect v3", group: "rfc-core" },
  { slug: "rfc-0029", file: "rfcs/0029-a2a-conversations-principals-commands.md", title: "0029 · A2A conversations & principals", group: "rfc-core" },
  { slug: "rfc-0025", file: "rfcs/0025-durable-state-and-store-adapters.md", title: "0025 · Durable state & store", group: "rfc-core" },
  { slug: "rfc-0028", file: "rfcs/0028-tools-registry-and-internal-tools.md", title: "0028 · Tools registry", group: "rfc-core" },
  { slug: "rfc-0030", file: "rfcs/0030-config-schema-v2.md", title: "0030 · Config schema v2", group: "rfc-core" },
  { slug: "rfc-0031", file: "rfcs/0031-endpoint-authentication.md", title: "0031 · Endpoint authentication", group: "rfc-core" },
  { slug: "rfc-0032", file: "rfcs/0032-interface-and-observation-plane.md", title: "0032 · Interface & observation plane", group: "rfc-core" },
  { slug: "rfc-0033", file: "rfcs/0033-file-store-and-instance-identity.md", title: "0033 · File store & instance identity", group: "rfc-core" },
  { slug: "rfc-0034", file: "rfcs/0034-instruction-documents-and-directives.md", title: "0034 · Instruction documents & directives", group: "rfc-core" },
  { slug: "rfc-0035", file: "rfcs/0035-event-streams.md", title: "0035 · Event streams", group: "rfc-core", tag: "draft" },
  {
    slug: "rfc-0036",
    file: "rfcs/0036-subagent-templates.md",
    title: "0036 · Subagent templates & instance children",
    group: "rfc-core",
    tag: "draft",
    blurb:
      "Operator-declared subagent templates whose instruction is a full instance definition — spawn a supervised child desk with its own workflows, signals and streams; the model fills declared params only.",
  },
  {
    slug: "rfc-0038",
    file: "rfcs/0038-system-prompt-template.md",
    title: "0038 · The system-prompt template",
    group: "rfc-core",
    blurb:
      "The system prompt becomes data plus a template — loops, conditions and limits over the agent's environment, ordered so provider prefix caching actually hits.",
  },
  {
    slug: "rfc-0037",
    file: "rfcs/0037-service-catalog-and-egress-policy.md",
    title: "0037 · Service catalog & egress policy",
    group: "rfc-core",
    tag: "draft",
    blurb:
      "A services: catalog of the external services a deployment may use — shared credentials, authoritative trifecta tags, tool ceilings — with security.egress: closed enforcing it at dial time.",
  },
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
  { slug: "rfc-0013", file: "rfcs/0013-deferred-v2-surface.md", title: "0013 · Deferred v2 surface", group: "rfc-foundation" },
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
  // A registry entry whose file is missing must not take the whole build down
  // with it — the page renders as not-found and every other doc still ships.
  try {
    const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
    return { ...entry, raw };
  } catch {
    return null;
  }
}
