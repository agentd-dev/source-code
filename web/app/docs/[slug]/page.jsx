import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { DOCS, readDoc } from "../../../lib/docs";

export function generateStaticParams() {
  return DOCS.map((d) => ({ slug: d.slug }));
}

export function generateMetadata({ params }) {
  return { title: `agentd docs — ${params.slug}` };
}

export default function DocPage({ params }) {
  const doc = readDoc(params.slug);

  return (
    <main className="mx-auto flex max-w-6xl gap-8 px-4 py-10">
      <aside className="hidden w-52 shrink-0 md:block">
        <div className="frame sticky top-20">
          <div className="frame-title">
            <span className="dot" /> ls docs/
          </div>
          <ul className="p-3 text-sm leading-7">
            {DOCS.map((d) => (
              <li key={d.slug}>
                <Link
                  href={`/docs/${d.slug}/`}
                  className={
                    d.slug === params.slug
                      ? "text-[var(--accent)]"
                      : "text-[var(--dim)] hover:text-[var(--accent)]"
                  }
                >
                  {d.slug === params.slug ? "▸ " : "  "}
                  {d.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
      </aside>
      <article className="prose prose-invert prose-agentd min-w-0 max-w-none flex-1 prose-pre:text-xs sm:prose-pre:text-sm">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{doc.raw}</ReactMarkdown>
      </article>
    </main>
  );
}
