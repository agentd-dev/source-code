import Link from "next/link";
import { GROUPS, docsInGroup } from "../../lib/docs";

export const metadata = {
  title: "Documentation — agentd",
  description:
    "Guides, concepts, operations and reference for agentd — the minimal, MCP-native agent runtime.",
  alternates: { canonical: "/docs/" },
};

function Tag({ tag }) {
  if (!tag) return null;
  return <span className={`doc-tag doc-tag-${tag.replace(/\W/g, "")}`}>{tag}</span>;
}

/** The three paths into the documentation, for a reader who has not chosen yet. */
const PATHS = [
  {
    href: "/docs/getting-started/",
    kicker: "new here",
    title: "Run it in five minutes",
    body: "Install the binary, write a small config, and get a first answer out of a real model. Then turn the same instruction into a daemon.",
  },
  {
    href: "/docs/architecture/",
    kicker: "evaluating it",
    title: "Understand the design",
    body: "Two loops, one static binary, and the protocols from their own SDKs. Start with the architecture, then read the harness for what keeps an agent bounded.",
  },
  {
    href: "/docs/coding-agent/",
    kicker: "building with it",
    title: "Assemble a coding agent",
    body: "A pair-programming daemon you host: the tools it gets, the fence around them, and the approval flow that reaches every screen.",
  },
];

export default function DocsIndex() {
  // Only groups that actually hold a doc get a heading — an empty group would
  // otherwise render a title over nothing.
  const groups = GROUPS.filter((g) => docsInGroup(g.id).length > 0);

  return (
    <main className="mx-auto max-w-[70rem] px-4 py-14">
      <div className="eyebrow mb-3">documentation</div>
      <h1 className="text-3xl font-bold tracking-tight text-[var(--fg-strong)] sm:text-4xl">
        Everything about running an agent you own
      </h1>
      <p className="mt-4 max-w-2xl text-lg text-[var(--dim)]">
        Guides, concept articles and reference — rendered straight from the repository&apos;s
        markdown, so what you read is what the runtime ships with.
      </p>

      <div className="mt-10 grid gap-4 md:grid-cols-3">
        {PATHS.map((p) => (
          <Link key={p.href} href={p.href} className="panel lift block p-5">
            <div className="font-mono text-xs text-[var(--green)]">{p.kicker}</div>
            <h2 className="mt-2 text-base font-semibold text-[var(--fg-strong)]">{p.title}</h2>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">{p.body}</p>
          </Link>
        ))}
      </div>

      {groups.map((g) => (
        <section key={g.id} className="mt-14">
          <h2 className="mb-4 text-sm font-semibold uppercase tracking-[0.12em] text-[var(--green)]">
            {g.title}
          </h2>
          <div className="grid gap-3 sm:grid-cols-2">
            {docsInGroup(g.id).map((d) => (
              <Link key={d.slug} href={`/docs/${d.slug}/`} className="panel lift block p-4">
                <div className="flex items-baseline justify-between gap-2">
                  <h3 className="font-semibold text-[var(--fg-strong)]">{d.title}</h3>
                  <Tag tag={d.tag} />
                </div>
                {d.blurb && (
                  <p className="mt-1.5 text-sm leading-relaxed text-[var(--dim)]">{d.blurb}</p>
                )}
              </Link>
            ))}
          </div>
        </section>
      ))}

    </main>
  );
}
