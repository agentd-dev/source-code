import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { DOCS, readDoc, slugForFile } from "../../../lib/docs";

export function generateStaticParams() {
  return DOCS.map((d) => ({ slug: d.slug }));
}

export async function generateMetadata({ params }) {
  const { slug } = await params;
  const doc = DOCS.find((d) => d.slug === slug);
  return { title: `agentd docs — ${doc ? doc.title : slug}` };
}

// Map markdown links to the right destination: a `*.md` doc the site hosts → its
// on-site route; everything else → GitHub or the external URL verbatim.
function MdLink({ href, children }) {
  if (!href) return <span>{children}</span>;
  if (/^(https?:|mailto:)/.test(href)) {
    return (
      <a href={href} target="_blank" rel="noreferrer">
        {children}
      </a>
    );
  }
  if (href.startsWith("#")) return <a href={href}>{children}</a>;

  const [p, hash] = href.split("#");
  const name = p.split("/").pop();
  const slug = name && name.endsWith(".md") ? slugForFile(name) : null;
  if (slug) {
    return <Link href={`/docs/${slug}/${hash ? "#" + hash : ""}`}>{children}</Link>;
  }
  const clean = p.replace(/^(\.\.\/|\.\/)+/, "");
  return (
    <a
      href={`https://github.com/agentd-dev/source-code/blob/main/${clean}${hash ? "#" + hash : ""}`}
      target="_blank"
      rel="noreferrer"
    >
      {children}
    </a>
  );
}

export default async function DocPage({ params }) {
  const { slug } = await params;
  const doc = readDoc(slug);
  if (!doc) {
    return (
      <main className="mx-auto max-w-3xl px-4 py-20 text-[var(--dim)]">
        <p>doc not found.</p>
        <Link href="/docs/overview/" className="text-[var(--green)]">
          ← back to docs
        </Link>
      </main>
    );
  }

  return (
    <main className="mx-auto flex max-w-6xl flex-col gap-6 px-4 py-10 lg:flex-row lg:gap-8">
      {/* mobile / tablet: a collapsible doc switcher (the sidebar is lg-only) */}
      <details className="panel lg:hidden">
        <summary className="panel-title cursor-pointer list-none">
          <span className="pulse" /> docs — {doc.title}
        </summary>
        <ul className="grid grid-cols-1 gap-1 p-3 text-sm sm:grid-cols-2">
          {DOCS.map((d) => (
            <li key={d.slug}>
              <Link
                href={`/docs/${d.slug}/`}
                className={
                  d.slug === slug
                    ? "text-[var(--fg-strong)]"
                    : "text-[var(--dim)] hover:text-[var(--green)]"
                }
              >
                {d.slug === slug ? "▸ " : "  "}
                {d.title}
              </Link>
            </li>
          ))}
        </ul>
      </details>
      <aside className="hidden w-56 shrink-0 lg:block">
        <div className="panel sticky top-20">
          <div className="panel-title">
            <span className="pulse" /> ls docs/
          </div>
          <ul className="p-3 text-sm leading-7">
            {DOCS.map((d) => (
              <li key={d.slug}>
                <Link
                  href={`/docs/${d.slug}/`}
                  className={
                    d.slug === slug
                      ? "text-[var(--fg-strong)]"
                      : "text-[var(--dim)] hover:text-[var(--green)]"
                  }
                >
                  {d.slug === slug ? "▸ " : "  "}
                  {d.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
      </aside>
      <article className="prose prose-invert prose-agent min-w-0 max-w-none flex-1 prose-pre:text-xs sm:prose-pre:text-sm">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={{ a: MdLink }}>
          {doc.raw}
        </ReactMarkdown>
      </article>
    </main>
  );
}
