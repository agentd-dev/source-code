import Link from "next/link";
import { DOCS } from "../../lib/docs";

export const metadata = { title: "agentd docs" };

export default function DocsIndex() {
  const guides = DOCS.filter((d) => !d.slug.startsWith("rfc-"));
  const rfcs = DOCS.filter((d) => d.slug.startsWith("rfc-"));

  return (
    <main className="mx-auto max-w-4xl px-4 py-14">
      <div className="eyebrow mb-3">documentation</div>
      <h1 className="text-3xl font-bold text-[var(--fg-strong)]">docs</h1>
      <p className="mt-3 max-w-2xl text-[var(--dim)]">
        Rendered straight from the repository&apos;s authoritative markdown — the same{" "}
        <span className="kbd">docs/</span> and <span className="kbd">rfcs/</span> the runtime ships
        with.
      </p>

      <div className="mt-10 grid gap-8 sm:grid-cols-2">
        <div>
          <div className="panel-title mb-3 border-0 px-0 text-[var(--green)]">guides</div>
          <ul className="space-y-1.5">
            {guides.map((d) => (
              <li key={d.slug}>
                <Link
                  href={`/docs/${d.slug}/`}
                  className="text-[var(--fg)] hover:text-[var(--green)]"
                >
                  <span className="text-[var(--dimmer)]">→</span> {d.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
        <div>
          <div className="panel-title mb-3 border-0 px-0 text-[var(--green)]">rfcs</div>
          <ul className="space-y-1.5">
            {rfcs.map((d) => (
              <li key={d.slug}>
                <Link
                  href={`/docs/${d.slug}/`}
                  className="text-[var(--fg)] hover:text-[var(--green)]"
                >
                  <span className="text-[var(--dimmer)]">→</span> {d.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
      </div>
    </main>
  );
}
