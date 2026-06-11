import fs from "node:fs";
import path from "node:path";

// The use-case catalog renders the repo's authoritative markdown from
// docs/use-cases/ — same single-source-of-truth rule as lib/docs.js.
const ROOT = path.join(process.cwd(), "..");

export const USE_CASES = [
  {
    slug: "voice-receptionist",
    file: "docs/use-cases/voice-receptionist.md",
    title: "ai voice receptionist",
  },
  {
    slug: "lead-enrichment",
    file: "docs/use-cases/lead-enrichment.md",
    title: "lead deep-research",
  },
  {
    slug: "support-triage",
    file: "docs/use-cases/support-triage.md",
    title: "support triage",
  },
  {
    slug: "invoice-approval",
    file: "docs/use-cases/invoice-approval.md",
    title: "invoice approval",
  },
  {
    slug: "exec-digest",
    file: "docs/use-cases/exec-digest.md",
    title: "executive digest",
  },
  {
    slug: "churn-monitor",
    file: "docs/use-cases/churn-monitor.md",
    title: "churn early-warning",
  },
  {
    slug: "content-localization",
    file: "docs/use-cases/content-localization.md",
    title: "content localization",
  },
  {
    slug: "incident-copilot",
    file: "docs/use-cases/incident-copilot.md",
    title: "incident copilot",
  },
  {
    slug: "contract-review",
    file: "docs/use-cases/contract-review.md",
    title: "contract review",
  },
  {
    slug: "resume-screening",
    file: "docs/use-cases/resume-screening.md",
    title: "resume screening",
  },
  {
    slug: "fraud-review",
    file: "docs/use-cases/fraud-review.md",
    title: "order fraud review",
  },
  {
    slug: "compliance-evidence",
    file: "docs/use-cases/compliance-evidence.md",
    title: "compliance evidence",
  },
  {
    slug: "inbox-concierge",
    file: "docs/use-cases/inbox-concierge.md",
    title: "inbox concierge",
  },
  {
    slug: "data-reconciliation",
    file: "docs/use-cases/data-reconciliation.md",
    title: "data reconciliation",
  },
  {
    slug: "gap-analysis",
    file: "docs/use-cases/GAP-ANALYSIS.md",
    title: "capability gap analysis",
  },
];

export function readUseCase(slug) {
  const entry = USE_CASES.find((d) => d.slug === slug);
  if (!entry) return null;
  const raw = fs.readFileSync(path.join(ROOT, entry.file), "utf8");
  return { ...entry, raw };
}

export function readIndex() {
  return fs.readFileSync(path.join(ROOT, "docs/use-cases/README.md"), "utf8");
}

// Map the markdown's repo-relative links onto site routes so the
// catalog is navigable in the browser, not just on GitHub.
//  - sibling articles ("support-triage.md", "GAP-ANALYSIS.md#x") → /use-cases/<slug>/
//  - parent docs ("../CONFORMANCE.md") → /docs/<slug>/ when registered
//  - repo files ("../../examples/…") → the GitHub blob
const REPO_BLOB = "https://github.com/agentd-dev/source-code/blob/main";
const PARENT_DOC_SLUGS = {
  "README.md": "overview",
  "quickstart.md": "quickstart",
  "architecture.md": "architecture",
  "capabilities.md": "capabilities",
  "configuration.md": "configuration",
  "operations.md": "operations",
  "maturity.md": "maturity",
  "SAMPLES.md": "samples",
  "CONFORMANCE.md": "conformance",
  "ROADMAP.md": "roadmap",
};

export function rewriteHref(href) {
  if (!href || href.startsWith("http") || href.startsWith("#")) return href;
  const [target, anchor] = href.split("#");
  const suffix = anchor ? `#${anchor}` : "";
  if (target.startsWith("../../")) {
    return `${REPO_BLOB}/${target.replace(/^(\.\.\/)+/, "")}`;
  }
  if (target.startsWith("../")) {
    const name = target.slice(3);
    const slug = PARENT_DOC_SLUGS[name];
    return slug ? `/docs/${slug}/${suffix}` : `${REPO_BLOB}/docs/${name}`;
  }
  if (target.endsWith(".md")) {
    const base = target.replace(/\.md$/, "");
    if (base === "README") return `/use-cases/${suffix}`;
    const entry = USE_CASES.find(
      (d) => d.slug === base.toLowerCase() || d.file.endsWith(`/${target}`),
    );
    if (entry) return `/use-cases/${entry.slug}/${suffix}`;
  }
  return href;
}
