import Link from "next/link";
import ReactMarkdown from "react-markdown";
import tuiFrames from "../../../lib/tui-frames.json";
import remarkGfm from "remark-gfm";
import { DOCS, GROUPS, docsInGroup, readDoc, slugForFile } from "../../../lib/docs";
import Mermaid from "../../components/Mermaid";

export function generateStaticParams() {
  return DOCS.map((d) => ({ slug: d.slug }));
}

export async function generateMetadata({ params }) {
  const { slug } = await params;
  const doc = DOCS.find((d) => d.slug === slug);
  return { title: doc ? `${doc.title} — agentd` : `agentd docs — ${slug}` };
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
  // ```tui blocks are REAL frames, rendered by the shipped TUI against a mirror
  // and captured by interface/tools/frames.mjs. Giving them terminal chrome
  // says "this is the program", which a bare code block does not — and the
  // frames regenerate from the code, so they cannot drift into fiction.
  //
  // The fence itself carries STRIPPED text (readable on GitHub, diff-stable);
  // the coloured original lives in web/lib/tui-frames.json keyed by the
  // fence's title, so the site shows the program the way a terminal does.
  if (cls.includes("language-tui")) {
    const code = child.props.children;
    const text = (Array.isArray(code) ? code.join("") : String(code)).replace(/\n$/, "");
    const [first, ...rest] = text.split("\n");
    const titled = first.startsWith("# ");
    const title = titled ? first.slice(2) : "agentd tui";
    const ansi = tuiFrames[title.trim()];
    return (
      <div className="term term-doc">
        <div className="panel-title">
          <span className="dots">
            <i />
            <i />
            <i />
          </span>
          <span className="ml-1">{title}</span>
        </div>
        <pre>{ansi ? ansiSpans(ansi) : titled ? rest.join("\n") : text}</pre>
      </div>
    );
  }
  return <pre>{children}</pre>;
}

/** SGR colour names for the 16-colour codes chalk emits. */
const ANSI_NAMES = {
  30: "black", 31: "red", 32: "green", 33: "yellow",
  34: "blue", 35: "magenta", 36: "cyan", 37: "white",
  90: "bblack", 91: "bred", 92: "bgreen", 93: "byellow",
  94: "bblue", 95: "bmagenta", 96: "bcyan", 97: "bwhite",
};

/**
 * A captured TUI frame's ANSI escapes, as styled spans.
 *
 * Runs at build time (this is a server component), so the browser gets plain
 * HTML. Deliberately covers only what Ink actually emits — the 16 SGR colours,
 * bold, dim, inverse, and their resets — rather than a whole terminal
 * emulator: anything unrecognised drops its escape and keeps its text, so a
 * new code can never make a frame unreadable, only uncoloured.
 */
function ansiSpans(text) {
  const out = [];
  const state = { fg: null, bold: false, dim: false, inv: false };
  let buf = "";
  let key = 0;
  const flush = () => {
    if (!buf) return;
    const classes = [];
    if (state.fg) classes.push(`an-${state.fg}`);
    if (state.bold) classes.push("an-b");
    if (state.dim) classes.push("an-d");
    if (state.inv) classes.push("an-inv");
    out.push(
      classes.length ? (
        <span key={key++} className={classes.join(" ")}>
          {buf}
        </span>
      ) : (
        buf
      ),
    );
    buf = "";
  };
  for (const part of text.split(/(\u001b\[[0-9;]*m)/)) {
    const m = /^\u001b\[([0-9;]*)m$/.exec(part);
    if (!m) {
      buf += part;
      continue;
    }
    flush();
    for (const code of (m[1] || "0").split(";").map(Number)) {
      if (code === 0) Object.assign(state, { fg: null, bold: false, dim: false, inv: false });
      else if (code === 1) state.bold = true;
      else if (code === 2) state.dim = true;
      else if (code === 7) state.inv = true;
      else if (code === 22) (state.bold = false), (state.dim = false);
      else if (code === 27) state.inv = false;
      else if (code === 39) state.fg = null;
      else if (ANSI_NAMES[code]) state.fg = ANSI_NAMES[code];
    }
  }
  flush();
  return out;
}

/** Slugify a heading the same way rehype-slug would, so the TOC links land. */
function slugify(text) {
  return String(text)
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .replace(/\s+/g, "-");
}

function headingText(children) {
  const flat = (n) =>
    typeof n === "string"
      ? n
      : Array.isArray(n)
        ? n.map(flat).join("")
        : n?.props?.children
          ? flat(n.props.children)
          : "";
  return flat(children);
}

const H = (level) =>
  function Heading({ children }) {
    const Tag = `h${level}`;
    return <Tag id={slugify(headingText(children))}>{children}</Tag>;
  };

/** Stamp every `td` with its column's header text, at BUILD time.
 *
 * On a phone a four-column reference table has nowhere to go: squeeze it and
 * the identifiers shatter mid-word, scroll it and the column saying what the
 * thing DOES sits off-screen. The fix is to stop being a table below the
 * breakpoint and become one labelled block per row — which needs each cell to
 * know its column name. A rehype plugin does that during static rendering, so
 * nothing ships to the browser and the page stays a server component. */
function rehypeTableLabels() {
  const text = (n) =>
    n.type === "text" ? n.value || "" : (n.children || []).map(text).join("");
  const walk = (node) => {
    if (node.tagName === "table") {
      const kids = node.children || [];
      const head = kids.find((c) => c.tagName === "thead");
      const headRow = (head?.children || []).find((c) => c.tagName === "tr");
      const labels = (headRow?.children || [])
        .filter((c) => c.tagName === "th")
        .map((c) => text(c).trim());
      for (const section of kids) {
        for (const row of section.children || []) {
          if (row.tagName !== "tr") continue;
          let i = 0;
          for (const cell of row.children || []) {
            if (cell.tagName !== "td") continue;
            const label = labels[i++];
            if (label) {
              cell.properties = { ...(cell.properties || {}), "data-label": label };
            }
          }
        }
      }
    }
    for (const child of node.children || []) walk(child);
  };
  return (tree) => walk(tree);
}

const MD = { a: MdLink, pre: Pre, h2: H(2), h3: H(3), h4: H(4) };

/** Pull `##` / `###` headings out of the raw markdown for the on-page rail. */
function outline(raw) {
  const out = [];
  let inFence = false;
  for (const line of raw.split("\n")) {
    if (/^\s*```/.test(line)) inFence = !inFence;
    if (inFence) continue;
    const m = /^(#{2,3})\s+(.*)$/.exec(line);
    if (!m) continue;
    const text = m[2].replace(/`/g, "").replace(/\[([^\]]+)\]\([^)]*\)/g, "$1").trim();
    out.push({ level: m[1].length, text, id: slugify(text) });
  }
  return out;
}

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

/**
 * Drop the document's leading `# Title` — the page renders its own header, and
 * two titles in a row reads like a mistake.
 */
function stripTitle(raw) {
  return raw.replace(/^\s*#\s+.*\n+/, "");
}

/** Reading order across groups, for prev/next. */
function readingOrder() {
  return GROUPS.flatMap((g) => docsInGroup(g.id));
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
  const toc = outline(doc.raw);
  const order = readingOrder();
  const i = order.findIndex((d) => d.slug === slug);
  const prev = i > 0 ? order[i - 1] : null;
  const next = i >= 0 && i < order.length - 1 ? order[i + 1] : null;

  return (
    <div className="docs-shell py-10">
      {/* mobile: a compact grouped switcher */}
      <details className="panel lg:hidden">
        <summary className="panel-title cursor-pointer list-none">
          <span className="pulse" /> Documentation — {doc.title}
        </summary>
        <nav aria-label="documentation" className="doc-nav p-3">
          <NavList activeSlug={slug} />
        </nav>
      </details>

      <aside className="docs-side">
        <nav aria-label="documentation" className="doc-nav sticky-rail">
          <NavList activeSlug={slug} />
        </nav>
      </aside>

      <article className="min-w-0">
        <header className="mb-8">
          <div className="eyebrow">{group ? group.title : "docs"}</div>
          <div className="mt-2 flex items-baseline gap-3">
            <h1 className="text-3xl font-bold tracking-tight text-[var(--fg-strong)]">
              {doc.title}
            </h1>
            {doc.tag && <Tag tag={doc.tag} />}
          </div>
        </header>

        <div className="prose prose-agent max-w-none">
          <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeTableLabels]} components={MD}>
            {stripTitle(doc.raw)}
          </ReactMarkdown>
        </div>

        <nav className="mt-14 grid gap-3 border-t border-[var(--line)] pt-6 sm:grid-cols-2">
          {prev ? (
            <Link href={`/docs/${prev.slug}/`} className="panel lift p-4">
              <div className="text-xs text-[var(--dim)]">← Previous</div>
              <div className="mt-1 text-sm font-medium text-[var(--fg-strong)]">{prev.title}</div>
            </Link>
          ) : (
            <span />
          )}
          {next && (
            <Link href={`/docs/${next.slug}/`} className="panel lift p-4 sm:text-right">
              <div className="text-xs text-[var(--dim)]">Next →</div>
              <div className="mt-1 text-sm font-medium text-[var(--fg-strong)]">{next.title}</div>
            </Link>
          )}
        </nav>

        <div className="mt-8 text-xs text-[var(--dim)]">
          <a
            href={`https://github.com/agentd-dev/source-code/blob/main/${doc.file}`}
            target="_blank"
            rel="noreferrer"
            className="hover:text-[var(--green)]"
          >
            Edit this page on GitHub ↗
          </a>
        </div>
      </article>

      <aside className="docs-toc">
        {toc.length > 1 && (
          <nav aria-label="on this page" className="sticky-rail toc">
            <div className="toc-title">On this page</div>
            {toc.map((h, n) => (
              <a key={`${h.id}-${n}`} href={`#${h.id}`} className={h.level === 3 ? "lvl-3" : ""}>
                {h.text}
              </a>
            ))}
          </nav>
        )}
      </aside>
    </div>
  );
}
