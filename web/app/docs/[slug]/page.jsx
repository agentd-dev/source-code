import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { DOCS, GROUPS, docsInGroup, readDoc, slugForFile } from "../../../lib/docs";
import Mermaid from "../../components/Mermaid";

export function generateStaticParams() {
  return DOCS.map((d) => ({ slug: d.slug }));
}

export async function generateMetadata({ params }) {
  const { slug } = await params;
  const doc = DOCS.find((d) => d.slug === slug);
  return { title: `agentd docs — ${doc ? doc.title : slug}` };
}

// Map markdown links: a `*.md` doc the site hosts → its on-site route; a repo
// path → GitHub; an absolute URL → itself.
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

// Swap ```mermaid fenced blocks for a rendered diagram; everything else stays a
// normal <pre> the prose theme styles.
function Pre({ children }) {
  const child = Array.isArray(children) ? children[0] : children;
  const cls = child?.props?.className || "";
  if (cls.includes("language-mermaid")) {
    const code = child.props.children;
    const chart = (Array.isArray(code) ? code.join("") : String(code)).replace(/\n$/, "");
    return <Mermaid chart={chart} />;
  }
  return <pre>{children}</pre>;
}

const MD = { a: MdLink, pre: Pre };

function Tag({ tag }) {
  if (!tag) return null;
  return <span className={`doc-tag doc-tag-${tag.replace(/\W/g, "")}`}>{tag}</span>;
}

function NavList({ activeSlug }) {
  return GROUPS.map((g) => {
    const items = docsInGroup(g.id);
    if (!items.length) return null;
    const isRfc = g.id.startsWith("rfc");
    const activeHere = items.some((d) => d.slug === activeSlug);
    const list = (
      <ul className="doc-nav-list">
        {items.map((d) => (
          <li key={d.slug}>
            <Link
              href={`/docs/${d.slug}/`}
              aria-current={d.slug === activeSlug ? "page" : undefined}
              className={d.slug === activeSlug ? "doc-nav-link is-active" : "doc-nav-link"}
            >
              <span className="truncate">{d.title}</span>
              <Tag tag={d.tag} />
            </Link>
          </li>
        ))}
      </ul>
    );
    // RFC groups are long — collapse them unless the reader is inside one.
    if (isRfc) {
      return (
        <details key={g.id} className="doc-nav-group" open={activeHere}>
          <summary className="doc-nav-title">{g.title}</summary>
          {list}
        </details>
      );
    }
    return (
      <div key={g.id} className="doc-nav-group">
        <div className="doc-nav-title">{g.title}</div>
        {list}
      </div>
    );
  });
}

export default async function DocPage({ params }) {
  const { slug } = await params;
  const doc = readDoc(slug);
  if (!doc) {
    return (
      <main className="mx-auto max-w-3xl px-4 py-20 text-[var(--dim)]">
        <p>doc not found.</p>
        <Link href="/docs/" className="text-[var(--green)]">
          ← back to docs
        </Link>
      </main>
    );
  }

  const group = GROUPS.find((g) => g.id === doc.group);

  return (
    <div className="mx-auto flex max-w-6xl flex-col gap-8 px-4 py-10 lg:flex-row">
      {/* mobile: a compact grouped switcher */}
      <details className="panel lg:hidden">
        <summary className="panel-title cursor-pointer list-none">
          <span className="pulse" /> docs — {doc.title}
        </summary>
        <nav aria-label="docs" className="doc-nav p-3">
          <NavList activeSlug={slug} />
        </nav>
      </details>

      {/* desktop: sticky grouped sidebar */}
      <aside className="hidden w-60 shrink-0 lg:block">
        <nav aria-label="docs" className="doc-nav sticky top-20 max-h-[calc(100vh-6rem)] overflow-y-auto pr-2">
          <NavList activeSlug={slug} />
        </nav>
      </aside>

      <article className="min-w-0 flex-1">
        <div className="mb-6 border-b border-[var(--line)] pb-4">
          <div className="text-xs uppercase tracking-[0.18em] text-[var(--green)]">
            {group ? group.title : "docs"}
          </div>
          <div className="mt-1 flex items-baseline gap-3">
            <h1 className="text-2xl font-bold text-[var(--fg-strong)]">{doc.title}</h1>
            {doc.tag && <Tag tag={doc.tag} />}
          </div>
          <a
            className="mt-2 inline-block text-xs text-[var(--dim)] hover:text-[var(--green)]"
            href={`https://github.com/agentd-dev/source-code/blob/main/${doc.file}`}
            target="_blank"
            rel="noreferrer"
          >
            view source · {doc.file} ↗
          </a>
        </div>
        <div className="prose prose-invert prose-agent max-w-none prose-pre:text-xs sm:prose-pre:text-sm">
          <ReactMarkdown remarkPlugins={[remarkGfm]} components={MD}>
            {doc.raw}
          </ReactMarkdown>
        </div>
      </article>
    </div>
  );
}
