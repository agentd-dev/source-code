import Link from "next/link";
import { GROUPS, docsInGroup } from "../../lib/docs";

export const metadata = { title: "Documentation — agentd" };

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
    body: "Two loops, one binary, three dependencies. Start with the architecture, then read the harness for what keeps an agent bounded.",
  },
  {
    href: "/docs/coding-agent/",
    kicker: "building with it",
    title: "Assemble a coding agent",
    body: "A pair-programming daemon you host: the tools it gets, the fence around them, and the approval flow that reaches every screen.",
  },
];

export default function DocsIndex() {
  const guideGroups = GROUPS.filter((g) => !g.id.startsWith("rfc"));
  const rfcGroups = GROUPS.filter((g) => g.id.startsWith("rfc"));

  return (
    <main className="mx-auto max-w-[70rem] px-4 py-14">
      <div className="eyebrow mb-3">documentation</div>
      <h1 className="text-3xl font-bold tracking-tight text-[var(--fg-strong)] sm:text-4xl">
        Everything about running an agent you own
      </h1>
      <p className="mt-4 max-w-2xl text-lg text-[var(--dim)]">
        Guides, concept articles and the normative specifications — rendered straight from the
        repository&apos;s markdown, so what you read is what the runtime ships with.
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

      {guideGroups.map((g) => (
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

      <section className="mt-16">
        <h2 className="mb-1 text-sm font-semibold uppercase tracking-[0.12em] text-[var(--green)]">
          Specifications
        </h2>
        <p className="mb-5 max-w-2xl text-sm text-[var(--dim)]">
          The normative specs behind the implementation. Read these when you need the exact contract
          — the guides above explain the same material for people who need to use it rather than
          re-implement it.
        </p>
        <div className="grid gap-8 sm:grid-cols-2">
          {rfcGroups.map((g) => (
            <div key={g.id}>
              <div className="mb-2 font-mono text-xs uppercase tracking-wider text-[var(--dim)]">
                {g.title}
              </div>
              <ul className="space-y-1.5">
                {docsInGroup(g.id).map((d) => (
                  <li key={d.slug}>
                    <Link
                      href={`/docs/${d.slug}/`}
                      className="inline-flex items-center gap-2 text-sm text-[var(--fg)] hover:text-[var(--green)]"
                    >
                      <span className="text-[var(--dimmer)]">→</span> {d.title}
                      <Tag tag={d.tag} />
                    </Link>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
      </section>
    </main>
  );
}
