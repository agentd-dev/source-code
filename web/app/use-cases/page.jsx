import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { readIndex, rewriteHref } from "../../lib/useCases";

export const metadata = {
  title: "agentd — business-automation use cases",
  description:
    "Fourteen working patterns for putting a bounded AI agent on real " +
    "business work — each a general-audience article plus a validated " +
    "sample workflow.",
};

const mdComponents = {
  a: ({ href, children, ...props }) => (
    <a href={rewriteHref(href)} {...props}>
      {children}
    </a>
  ),
};

export default function UseCasesIndex() {
  return (
    <main className="mx-auto max-w-4xl px-4 py-10">
      <article className="prose prose-invert prose-agentd min-w-0 max-w-none prose-pre:text-xs sm:prose-pre:text-sm">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={mdComponents}>
          {readIndex()}
        </ReactMarkdown>
      </article>
    </main>
  );
}
