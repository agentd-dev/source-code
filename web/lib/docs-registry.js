// The documentation's information architecture, as data: the groups the
// sidebar shows (top level), the sections inside each group, and every page.
// Kept free of Node APIs so the client-side header menus can import it too;
// `docs.js` adds the server-side markdown loader on top.
//
// Generated from the docs IA; a page's `blurb` is the first sentence of its
// own opening, so the sidebar, the index and the search snippet say what the
// page says. `featured` pages (with a short `hint`) fill the header menus. An
// entry with an `href` lives elsewhere — an A2A extension spec at its URI.
export const GROUPS = [
  {
    "id": "product",
    "title": "Product",
    "menu": "Product",
    "blurb": "What agentd is, what it is for, and how it fits next to what you run today.",
    "sections": [
      {
        "id": "cases",
        "title": "Case studies"
      }
    ]
  },
  {
    "id": "start",
    "title": "Get started",
    "menu": "Get started",
    "blurb": "Install agentd and build your first agents, step by step.",
    "sections": []
  },
  {
    "id": "guides",
    "title": "Guides",
    "menu": "Guides",
    "blurb": "How to do one task, with a configuration that works.",
    "sections": [
      {
        "id": "models",
        "title": "Models"
      },
      {
        "id": "tools",
        "title": "Tools and instructions"
      },
      {
        "id": "workflows",
        "title": "Workflows"
      },
      {
        "id": "agents",
        "title": "Multi-agent"
      },
      {
        "id": "operate",
        "title": "Deploy and operate"
      },
      {
        "id": "secure",
        "title": "Security"
      },
      {
        "id": "integrate",
        "title": "Integrate"
      }
    ]
  },
  {
    "id": "concepts",
    "title": "Concepts",
    "menu": "Concepts",
    "blurb": "How agentd works, and why it is built that way.",
    "sections": []
  },
  {
    "id": "reference",
    "title": "Reference",
    "menu": "Reference",
    "blurb": "Every key, node, flag, event and wire detail, for lookup.",
    "sections": [
      {
        "id": "config",
        "title": "Configuration"
      },
      {
        "id": "workflows",
        "title": "Workflows"
      },
      {
        "id": "runtime",
        "title": "Runtime"
      },
      {
        "id": "protocols",
        "title": "Protocols"
      }
    ]
  }
];
export const DOCS = [
  {
    "slug": "overview",
    "file": "docs/README.md",
    "title": "Overview",
    "group": "product",
    "blurb": "What agentd is, who it is for, and where to start for your role.",
    "featured": true,
    "hint": "What agentd is, and where to start"
  },
  {
    "slug": "why-agentd",
    "file": "docs/why-agentd.md",
    "title": "Why agentd",
    "group": "product",
    "blurb": "The problem agentd solves, the principles behind it, and how it compares.",
    "featured": true,
    "hint": "The problem, the principles, the fit"
  },
  {
    "slug": "use-cases",
    "file": "docs/use-cases.md",
    "title": "Use cases",
    "group": "product",
    "blurb": "The shapes teams deploy — jobs, reactive daemons, pipelines, triage, agent teams — with examples.",
    "featured": true,
    "hint": "Shapes people deploy, with examples"
  },
  {
    "slug": "interoperability",
    "file": "docs/interoperability.md",
    "title": "Interoperability",
    "group": "product",
    "blurb": "The protocols, model providers, platforms and standards agentd works with."
  },
  {
    "slug": "security-overview",
    "file": "docs/security-overview.md",
    "title": "Security at a glance",
    "group": "product",
    "blurb": "How agentd contains an agent: no ambient tools, killable reasoning, approvals, audit.",
    "featured": true,
    "hint": "The containment model for reviewers"
  },
  {
    "slug": "faq",
    "file": "docs/faq.md",
    "title": "FAQ",
    "group": "product",
    "blurb": "Short answers to the questions evaluators and newcomers ask most."
  },
  {
    "slug": "two-person-company",
    "file": "docs/two-person-company.md",
    "title": "The two-person company",
    "group": "product",
    "section": "cases",
    "blurb": "An eleven-agent software company run by two people, worked end to end."
  },
  {
    "slug": "pid-1",
    "file": "docs/pid1.md",
    "title": "Edge and appliances: agentd as PID 1",
    "group": "product",
    "section": "cases",
    "blurb": "agentd as the init of a custom Linux for robots, appliances and kiosks."
  },
  {
    "slug": "install",
    "file": "docs/install.md",
    "title": "Install",
    "group": "start",
    "blurb": "The install script, release downloads, the container image, the npm clients and source builds.",
    "featured": true,
    "hint": "Binaries, image, npm, source"
  },
  {
    "slug": "getting-started",
    "file": "docs/getting-started.md",
    "title": "Quickstart",
    "group": "start",
    "blurb": "A first agent against a real model, a tool from an MCP server, then a daemon you attach to.",
    "featured": true,
    "hint": "A first agent, running, in minutes"
  },
  {
    "slug": "first-workflow",
    "file": "docs/first-workflow.md",
    "title": "Your first workflow",
    "group": "start",
    "blurb": "Write, validate and run a durable workflow — then kill it mid-run and watch it resume.",
    "featured": true,
    "hint": "Write, run, kill, resume"
  },
  {
    "slug": "coding-agent",
    "file": "docs/coding-agent.md",
    "title": "Build a coding agent",
    "group": "start",
    "blurb": "A pair-programming daemon on your repository, with approvals and a budget.",
    "featured": true,
    "hint": "A pair-programming daemon you host"
  },
  {
    "slug": "decision-triage",
    "file": "docs/decision-triage.md",
    "title": "Triage with a decision model",
    "group": "start",
    "blurb": "Typed decisions, decision guards and per-turn routing, on a support-ticket workflow."
  },
  {
    "slug": "connect-models",
    "file": "docs/connect-models.md",
    "title": "Connect model providers",
    "group": "guides",
    "section": "models",
    "blurb": "Connect OpenAI, Anthropic, Bedrock, Azure, Vertex, local servers and decision models.",
    "featured": true,
    "hint": "OpenAI, Anthropic, Bedrock, local, decisions"
  },
  {
    "slug": "models-and-roles-guide",
    "file": "docs/models-and-roles-guide.md",
    "title": "Choose models for each job",
    "group": "guides",
    "section": "models",
    "blurb": "Pick the model for each job: turns, thinking, decisions, triage, approvals, routing."
  },
  {
    "slug": "budgets",
    "file": "docs/budgets.md",
    "title": "Control spend with budgets",
    "group": "guides",
    "section": "models",
    "blurb": "Token windows, lifetime caps, degrade models and per-run limits."
  },
  {
    "slug": "connect-mcp",
    "file": "docs/connect-mcp.md",
    "title": "Connect MCP servers",
    "group": "guides",
    "section": "tools",
    "blurb": "Declare MCP servers, authenticate to them, scope their tools, and verify the connection.",
    "featured": true,
    "hint": "Declare, authenticate, verify"
  },
  {
    "slug": "react-to-events",
    "file": "docs/react-to-events.md",
    "title": "React to changes and events",
    "group": "guides",
    "section": "tools",
    "blurb": "Act when something changes: resource subscriptions, MCP Events, webhooks and streams."
  },
  {
    "slug": "instruction-documents",
    "file": "docs/instruction-documents.md",
    "title": "Write and ship instruction documents",
    "group": "guides",
    "section": "tools",
    "blurb": "Write an agent as one Markdown document, preview what the model reads, and ship it."
  },
  {
    "slug": "author-workflows",
    "file": "docs/author-workflows.md",
    "title": "Author workflows",
    "group": "guides",
    "section": "workflows",
    "blurb": "Triggers, data flow, fan-out, waits, guards and resilient remote calls, by example.",
    "featured": true,
    "hint": "Triggers, data, fan-out, resilience"
  },
  {
    "slug": "human-in-the-loop",
    "file": "docs/human-in-the-loop.md",
    "title": "Human in the loop",
    "group": "guides",
    "section": "workflows",
    "blurb": "Approvals, asks and human gates, and who can answer them."
  },
  {
    "slug": "troubleshooting",
    "file": "docs/troubleshooting.md",
    "title": "Troubleshooting",
    "group": "guides",
    "section": "workflows",
    "blurb": "Diagnose a refused config, a stalled run, a failed connection or a model error."
  },
  {
    "slug": "delegate-subagents",
    "file": "docs/delegate-subagents.md",
    "title": "Delegate to subagents",
    "group": "guides",
    "section": "agents",
    "blurb": "Hand bounded work to narrowed child agents and collect their results."
  },
  {
    "slug": "connect-agents",
    "file": "docs/connect-agents.md",
    "title": "Connect agents over A2A",
    "group": "guides",
    "section": "agents",
    "blurb": "Expose an agent over A2A, sign people in, and delegate to other agents."
  },
  {
    "slug": "deployment",
    "file": "docs/deployment.md",
    "title": "Deploy",
    "group": "guides",
    "section": "operate",
    "blurb": "Run agentd as a job, under systemd, or on Kubernetes.",
    "featured": true,
    "hint": "Jobs, systemd, Kubernetes"
  },
  {
    "slug": "operations",
    "file": "docs/operations.md",
    "title": "Operate a running daemon",
    "group": "guides",
    "section": "operate",
    "blurb": "Drain, pause, resume, cancel and reload a running daemon."
  },
  {
    "slug": "observability",
    "file": "docs/observability.md",
    "title": "Monitor in production",
    "group": "guides",
    "section": "operate",
    "blurb": "Logs, health probes, Prometheus metrics, OTLP traces and alerts."
  },
  {
    "slug": "scaling",
    "file": "docs/scaling.md",
    "title": "Scale out",
    "group": "guides",
    "section": "operate",
    "blurb": "Carry more work per instance, then spread it across replicas safely."
  },
  {
    "slug": "interface",
    "file": "docs/interface.md",
    "title": "Use the terminal and web UI",
    "group": "guides",
    "section": "operate",
    "blurb": "The terminal and web clients: attach, sign in, follow, approve, steer."
  },
  {
    "slug": "hosting-the-ui",
    "file": "docs/hosting-the-ui.md",
    "title": "Host the web UI",
    "group": "guides",
    "section": "operate",
    "blurb": "Serve the web UI from your own domain, and what the daemon must allow."
  },
  {
    "slug": "harden",
    "file": "docs/harden.md",
    "title": "Harden a deployment",
    "group": "guides",
    "section": "secure",
    "blurb": "A security checklist turned into a validated, hardened configuration.",
    "featured": true,
    "hint": "A checklist with validated config"
  },
  {
    "slug": "authentication",
    "file": "docs/authentication.md",
    "title": "Outbound credentials",
    "group": "guides",
    "section": "secure",
    "blurb": "Credentials for model providers, MCP servers and A2A peers: static, OAuth, AWS, SPIFFE."
  },
  {
    "slug": "aauth",
    "file": "docs/aauth.md",
    "title": "AAuth agent identity",
    "group": "guides",
    "section": "secure",
    "blurb": "Give an agent its own signed identity for AAuth-protected MCP servers."
  },
  {
    "slug": "build-a-client",
    "file": "docs/build-a-client.md",
    "title": "Build a client",
    "group": "guides",
    "section": "integrate",
    "blurb": "Build your own client on A2A and the extensions agentd declares."
  },
  {
    "slug": "embedding",
    "file": "docs/embedding.md",
    "title": "Embed agentd in Rust",
    "group": "guides",
    "section": "integrate",
    "blurb": "Build your own binary on agentd-core, with tools written in Rust."
  },
  {
    "slug": "act-at-companies",
    "file": "docs/act-at-companies.md",
    "title": "Act at companies for a person",
    "group": "guides",
    "section": "integrate",
    "blurb": "Act for one person at companies that publish their tools, agent and APIs under the Personal Agent Protocol draft.",
    "tag": "preview"
  },
  {
    "slug": "pay-with-x402",
    "file": "docs/pay-with-x402.md",
    "title": "Pay for things with x402",
    "group": "guides",
    "section": "integrate",
    "blurb": "Let a workflow pay with x402 for an HTTP resource, an MCP tool call or another agent's work, from a wallet agentd holds, only within a spend policy you declare.",
    "tag": "preview"
  },
  {
    "slug": "architecture",
    "file": "docs/architecture.md",
    "title": "Architecture",
    "group": "concepts",
    "blurb": "One binary, two kinds of process: how a run flows from configuration to result.",
    "featured": true,
    "hint": "Two loops, one binary"
  },
  {
    "slug": "harness",
    "file": "docs/harness.md",
    "title": "Supervision and containment",
    "group": "concepts",
    "blurb": "The supervisor that never talks to the model: process tree, kill ladder, recovery."
  },
  {
    "slug": "agent-loop",
    "file": "docs/agent-loop.md",
    "title": "How a turn runs",
    "group": "concepts",
    "blurb": "One turn end to end: context, model rounds, tool calls, termination."
  },
  {
    "slug": "modes-and-triggers",
    "file": "docs/modes-and-triggers.md",
    "title": "Jobs, daemons and triggers",
    "group": "concepts",
    "blurb": "One runtime, no modes: how jobs, daemons and triggers fit together."
  },
  {
    "slug": "workflows",
    "file": "docs/workflows.md",
    "title": "Workflows: how runs work",
    "group": "concepts",
    "blurb": "Durable step graphs: the run lifecycle, the scheduler, and when a graph beats a conversation.",
    "featured": true,
    "hint": "Durable DAGs, and why"
  },
  {
    "slug": "directives",
    "file": "docs/directives.md",
    "title": "Instruction documents",
    "group": "concepts",
    "blurb": "One reviewable file that defines an agent, and what a document may and may not declare."
  },
  {
    "slug": "intelligence",
    "file": "docs/intelligence.md",
    "title": "Models, providers and roles",
    "group": "concepts",
    "blurb": "Model kinds, providers and roles, fallback, structured generation and routing.",
    "featured": true,
    "hint": "Kinds, providers, roles, fallback"
  },
  {
    "slug": "mcp",
    "file": "docs/mcp.md",
    "title": "MCP: tools and context",
    "group": "concepts",
    "blurb": "Where tools and context come from, and how agentd connects to MCP servers."
  },
  {
    "slug": "a2a",
    "file": "docs/a2a.md",
    "title": "The A2A channel",
    "group": "concepts",
    "blurb": "How people and agents reach a running agentd: principals, conversations, tasks and the card."
  },
  {
    "slug": "subagents",
    "file": "docs/subagents.md",
    "title": "Subagents and delegation",
    "group": "concepts",
    "blurb": "Delegation as a process tree, and the trust boundary between parent and child."
  },
  {
    "slug": "security",
    "file": "docs/security.md",
    "title": "Security model",
    "group": "concepts",
    "blurb": "The threat model, the containment guarantees, and their limits.",
    "featured": true,
    "hint": "Threat model and guarantees"
  },
  {
    "slug": "configuration-model",
    "file": "docs/configuration-model.md",
    "title": "How configuration works",
    "group": "concepts",
    "blurb": "Layers, precedence, validation before side effects, secrets and hot reload."
  },
  {
    "slug": "experience",
    "file": "docs/experience.md",
    "title": "Built to be operated",
    "group": "concepts",
    "blurb": "Validation first, exit codes as an API, and telemetry you can filter."
  },
  {
    "slug": "why-rust",
    "file": "docs/why-rust.md",
    "title": "Engineering choices",
    "group": "concepts",
    "blurb": "Why Rust, the dependency discipline, and what the choices cost."
  },
  {
    "slug": "configuration",
    "file": "docs/configuration.md",
    "title": "Configuration reference",
    "group": "reference",
    "section": "config",
    "blurb": "Every configuration path: type, default, flag, environment variable and reload class.",
    "featured": true,
    "hint": "Every key, default and rule"
  },
  {
    "slug": "directives-reference",
    "file": "docs/directives-reference.md",
    "title": "Directive reference",
    "group": "reference",
    "section": "config",
    "blurb": "Instruction-document syntax, every directive kind, and the document-writable keys."
  },
  {
    "slug": "node-registry",
    "file": "docs/node-registry.md",
    "title": "Workflow node reference",
    "group": "reference",
    "section": "workflows",
    "blurb": "Every workflow node kind: fields, outputs and a sample."
  },
  {
    "slug": "workflow-language",
    "file": "docs/workflow-language.md",
    "title": "Workflow language reference",
    "group": "reference",
    "section": "workflows",
    "blurb": "Templates, durations, statuses, limits and validation rules for workflows."
  },
  {
    "slug": "cli",
    "file": "docs/cli.md",
    "title": "CLI, environment and exit codes",
    "group": "reference",
    "section": "runtime",
    "blurb": "Every flag, environment variable, subcommand and exit code.",
    "featured": true,
    "hint": "Flags, env vars, exit codes"
  },
  {
    "slug": "telemetry-reference",
    "file": "docs/telemetry-reference.md",
    "title": "Events and metrics reference",
    "group": "reference",
    "section": "runtime",
    "blurb": "Every log event and metric agentd emits."
  },
  {
    "slug": "features",
    "file": "docs/features.md",
    "title": "Build features and crates",
    "group": "reference",
    "section": "runtime",
    "blurb": "Cargo features, the release feature set, and the crates."
  },
  {
    "slug": "glossary",
    "file": "docs/glossary.md",
    "title": "Glossary",
    "group": "reference",
    "section": "runtime",
    "blurb": "The terms the documentation uses, defined in a sentence or two."
  },
  {
    "slug": "intelligence-reference",
    "file": "docs/intelligence-reference.md",
    "title": "Intelligence wire reference",
    "group": "reference",
    "section": "protocols",
    "blurb": "What agentd sends to each model wire, and how it reads the answer."
  },
  {
    "slug": "mcp-reference",
    "file": "docs/mcp-reference.md",
    "title": "MCP support reference",
    "group": "reference",
    "section": "protocols",
    "blurb": "MCP revisions, capabilities, negotiation and the events agentd logs."
  },
  {
    "slug": "external-signer",
    "file": "docs/external-signer.md",
    "title": "The external signer protocol",
    "group": "reference",
    "section": "protocols",
    "blurb": "The unix-socket protocol a signer serves when it, not agentd, holds the delegation keys."
  },
  {
    "slug": "a2a-reference",
    "file": "docs/a2a-reference.md",
    "title": "A2A API reference",
    "group": "reference",
    "section": "protocols",
    "blurb": "The A2A listener: routes, methods, errors, limits and card fields."
  },
  {
    "slug": "a2a-extensions",
    "file": "docs/a2a-extensions.md",
    "title": "A2A extensions",
    "group": "reference",
    "section": "protocols",
    "blurb": "The extensions agentd declares on top of A2A 1.0, and how a client activates them."
  },
  {
    "slug": "ext-command",
    "file": null,
    "title": "Command extension",
    "group": "reference",
    "section": "protocols",
    "href": "/a2a/ext/command/",
    "blurb": "The typed command DataPart: how a client runs an operation agentd declares."
  },
  {
    "slug": "ext-events",
    "file": null,
    "title": "Events extension",
    "group": "reference",
    "section": "protocols",
    "href": "/a2a/ext/events/",
    "blurb": "The observation feed a display client follows: frames, ordering and resume."
  },
  {
    "slug": "ext-task-annotations",
    "file": null,
    "title": "Task annotations extension",
    "group": "reference",
    "section": "protocols",
    "href": "/a2a/ext/task-annotations/",
    "blurb": "The facts agentd attaches to an A2A task for a client to render."
  },
  {
    "slug": "binding-jsonrpc-unix",
    "file": null,
    "title": "JSON-RPC over a unix socket",
    "group": "reference",
    "section": "protocols",
    "href": "/a2a/binding/jsonrpc-unix/",
    "blurb": "The A2A JSON-RPC binding on a unix socket, where the kernel vouches for the caller."
  },
  {
    "slug": "changelog",
    "file": "CHANGELOG.md",
    "title": "Changelog",
    "group": "reference",
    "section": "runtime",
    "blurb": "What changed in each release of agentd."
  }
];

export function docsInGroup(groupId) {
  return DOCS.filter((d) => d.group === groupId);
}

// Markdown basename (e.g. "configuration.md") → its slug, so a link between two
// docs can be rewritten to an on-site route. Keyed by BASENAME, not full path,
// because the source markdown links relatively ("./mcp.md", "../docs/mcp.md")
// and the basename is the only stable part. A basename the site does not host
// returns null, and the renderer falls back to a link into the repository.
const FILE_TO_SLUG = Object.fromEntries(
  DOCS.filter((d) => d.file).map((d) => [d.file.split("/").pop(), d.slug]),
);

export function slugForFile(name) {
  return FILE_TO_SLUG[name] ?? null;
}

/** The sections of a group, in their declared order, each with its docs. A doc
 *  with no `section` comes first, under an untitled section. Empty sections
 *  are left out. */
export function sectionsOf(groupId) {
  const group = GROUPS.find((g) => g.id === groupId);
  const docs = docsInGroup(groupId);
  const out = [];
  const loose = docs.filter((d) => !d.section);
  if (loose.length) out.push({ id: null, title: null, docs: loose });
  for (const sec of group?.sections ?? []) {
    const items = docs.filter((d) => d.section === sec.id);
    if (items.length) out.push({ ...sec, docs: items });
  }
  return out;
}

/** Every doc in reading order: group by group, section by section. */
export function readingOrder() {
  return GROUPS.flatMap((g) => sectionsOf(g.id).flatMap((s) => s.docs));
}
