// The spec page of an A2A extension or binding URI agentd publishes, served AT
// that URI: A2A's extension guidance says a third-party extension's
// specification should be hosted at its URI, so `https://agentd.dev/a2a/ext/
// command` is both the identifier a card declares and the page that defines
// it. Which URIs exist, and which markdown specifies each, comes from
// web/lib/extensions.json — generated from the binary by
// scripts/gen-schemas.sh — so the site cannot publish a page for a URI agentd
// does not declare, nor miss one it does.
import fs from "node:fs";
import path from "node:path";
import Link from "next/link";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import extensions from "../../lib/extensions.json";
import { slugForFile } from "../../lib/docs";

const ROOT = path.join(process.cwd(), "..");
const nameOf = (entry) => entry.path.split("/").pop();

/** The registry entries routed under one kind (`extension` or `binding`). */
export function specParams(kind) {
  return extensions.filter((e) => e.kind === kind).map((e) => ({ name: nameOf(e) }));
}

export function specEntry(kind, name) {
  return extensions.find((e) => e.kind === kind && nameOf(e) === name) || null;
}

function titleOf(raw) {
  const m = /^#\s+(.*)$/m.exec(raw);
  return m ? m[1].trim() : "agentd";
}

export function specMetadata(entry) {
  if (!entry) return { title: "agentd" };
  const raw = fs.readFileSync(path.join(ROOT, entry.spec), "utf8");
  const title = `${titleOf(raw)} — agentd`;
  const description = `The specification of ${entry.uri}, served at its URI.`;
  return {
    title,
    description,
    alternates: { canonical: `/${entry.path}/` },
    openGraph: { title, description, type: "article", url: `/${entry.path}/`, siteName: "agentd" },
  };
}

/**
 * A link in a spec, resolved the way a reader means it: another spec → the
 * URI it specifies; a doc the site hosts → its on-site page; anything else in
 * the repository → GitHub; an absolute URL → itself.
 */
function linkFor(entry) {
  return function SpecLink({ href, children }) {
    if (!href) return <span>{children}</span>;
    if (/^(https?:|mailto:)/.test(href)) return <a href={href}>{children}</a>;
    if (href.startsWith("#")) return <a href={href}>{children}</a>;
    const [p, hash] = href.split("#");
    const frag = hash ? `#${hash}` : "";
    const target = path.posix.normalize(path.posix.join(path.posix.dirname(entry.spec), p));
    const spec = extensions.find((e) => e.spec === target);
    if (spec) return <Link href={`/${spec.path}/${frag}`}>{children}</Link>;
    const slug = target.endsWith(".md") ? slugForFile(target.split("/").pop()) : null;
    if (slug) return <Link href={`/docs/${slug}/${frag}`}>{children}</Link>;
    return (
      <a
        href={`https://github.com/agentd-dev/source-code/blob/main/${target}${frag}`}
        target="_blank"
        rel="noreferrer"
      >
        {children}
      </a>
    );
  };
}

function slugify(text) {
  return String(text)
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .replace(/\s+/g, "-");
}

function flat(n) {
  if (typeof n === "string") return n;
  if (Array.isArray(n)) return n.map(flat).join("");
  return n?.props?.children ? flat(n.props.children) : "";
}

const H = (level) =>
  function Heading({ children }) {
    const Tag = `h${level}`;
    return <Tag id={slugify(flat(children))}>{children}</Tag>;
  };

export default function SpecPage({ entry }) {
  if (!entry) {
    return (
      <main className="mx-auto max-w-3xl px-4 py-20 text-[var(--dim)]">
        <p>no such specification.</p>
      </main>
    );
  }
  const raw = fs.readFileSync(path.join(ROOT, entry.spec), "utf8");
  const body = raw.replace(/^\s*#\s+.*\n+/, "");
  return (
    <main className="mx-auto max-w-3xl px-4 py-12">
      <header className="mb-8">
        <div className="eyebrow">{entry.kind === "binding" ? "A2A binding" : "A2A extension"}</div>
        <h1 className="mt-2 text-3xl font-bold tracking-tight text-[var(--fg-strong)]">
          {titleOf(raw)}
        </h1>
        <p className="mt-2 break-all text-sm text-[var(--dim)]">
          <code>{entry.uri}</code>
          {entry.schema && (
            <>
              {" · "}
              <a href={entry.schema}>schema.json</a>
            </>
          )}
        </p>
      </header>
      <article className="prose prose-agent max-w-none">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          components={{ a: linkFor(entry), h2: H(2), h3: H(3) }}
        >
          {body}
        </ReactMarkdown>
      </article>
      <div className="mt-10 text-xs text-[var(--dim)]">
        <a
          href={`https://github.com/agentd-dev/source-code/blob/main/${entry.spec}`}
          target="_blank"
          rel="noreferrer"
        >
          Edit this page on GitHub ↗
        </a>
      </div>
    </main>
  );
}
