import Link from "next/link";
import { GROUPS, docsInGroup } from "../../lib/docs";

export const metadata = { title: "agentd docs" };

function Tag({ tag }) {
  if (!tag) return null;
  return <span className={`doc-tag doc-tag-${tag.replace(/\W/g, "")}`}>{tag}</span>;
}

export default function DocsIndex() {
  const guideGroups = GROUPS.filter((g) => !g.id.startsWith("rfc"));
  const rfcGroups = GROUPS.filter((g) => g.id.startsWith("rfc"));

  return (
    <main className="mx-auto max-w-5xl px-4 py-14">
      <div className="eyebrow mb-3">documentation</div>
      <h1 className="text-3xl font-bold text-[var(--fg-strong)]">Docs</h1>
      <p className="mt-3 max-w-2xl text-[var(--dim)]">
        Rendered straight from the repository&apos;s authoritative markdown — the same{" "}
        <span className="kbd">docs/</span> and <span className="kbd">rfcs/</span> the runtime ships
        with. Diagrams are rendered inline; every page links to its source.
      </p>

      {guideGroups.map((g) => (
        <section key={g.id} className="mt-12">
          <h2 className="mb-4 text-sm font-semibold uppercase tracking-[0.14em] text-[var(--green)]">
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

      <section className="mt-14">
        <h2 className="mb-1 text-sm font-semibold uppercase tracking-[0.14em] text-[var(--green)]">
          RFCs
        </h2>
        <p className="mb-4 max-w-2xl text-sm text-[var(--dim)]">
          The normative specifications. The <span className="text-[var(--fg)]">2.0</span> set is the
          current design; the foundations remain normative for the base runtime, with{" "}
          <span className="doc-tag doc-tag-1x">1.x</span> marking superseded specs.
        </p>
        <div className="grid gap-8 sm:grid-cols-2">
          {rfcGroups.map((g) => (
            <div key={g.id}>
              <div className="mb-2 text-xs uppercase tracking-wider text-[var(--dim)]">{g.title}</div>
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
