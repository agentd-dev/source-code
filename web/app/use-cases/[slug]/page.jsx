import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { USE_CASES, readUseCase, rewriteHref } from "../../../lib/useCases";

export function generateStaticParams() {
  return USE_CASES.map((d) => ({ slug: d.slug }));
}

export function generateMetadata({ params }) {
  return { title: `agentd use cases — ${params.slug}` };
}

const mdComponents = {
  a: ({ href, children, ...props }) => (
    <a href={rewriteHref(href)} {...props}>
      {children}
    </a>
  ),
};

export default function UseCasePage({ params }) {
  const doc = readUseCase(params.slug);

  return (
    <main className="mx-auto flex max-w-6xl gap-8 px-4 py-10">
      <aside className="hidden w-52 shrink-0 md:block">
        <div className="frame sticky top-20">
          <div className="frame-title">
            <span className="dot" /> ls use-cases/
          </div>
          <ul className="p-3 text-sm leading-7">
            <li>
              <Link
                href="/use-cases/"
                className="text-[var(--dim)] hover:text-[var(--accent)]"
              >
                {"  "}index
              </Link>
            </li>
            {USE_CASES.map((d) => (
              <li key={d.slug}>
                <Link
                  href={`/use-cases/${d.slug}/`}
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
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={mdComponents}>
          {doc.raw}
        </ReactMarkdown>
      </article>
    </main>
  );
}
