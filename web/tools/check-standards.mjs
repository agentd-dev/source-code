// Every link of the landing page's standards strip is a link into the
// documentation that resolves: its page is a docs page the registry lists,
// and its anchor, when it has one, is the id of a heading that page was
// built with. Run after `next build` (npm's `postbuild`), so it reads the
// ids the renderer actually generated in out/, not a copy of its slug rule.
//
// The links checked are every entry of lib/standards.json and every link the
// built strip holds (its "and more" link included), and each entry must be
// one of the strip's links, so the strip cannot drift from the list it is
// checked against. A heading renamed in docs/ without the strip following it
// fails the site build here, rather than shipping a link that lands at the
// top of a page.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { DOCS } from "../lib/docs-registry.js";

const web = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const out = path.join(web, "out");
const standards = JSON.parse(fs.readFileSync(path.join(web, "lib/standards.json"), "utf8"));

const read = (rel) => {
  try {
    return fs.readFileSync(path.join(out, rel), "utf8");
  } catch {
    return null;
  }
};

/** The ids of a built page's headings. */
const headingIds = (html) =>
  new Set([...html.matchAll(/<h[1-6][^>]*\sid="([^"]*)"/g)].map((m) => m[1]));

/** The hrefs of the links in the built landing page's strip, or null without one. */
const stripHrefs = (html) => {
  const start = html.indexOf('aria-labelledby="standards-title"');
  if (start < 0) return null;
  const end = html.indexOf("</section>", start);
  const strip = html.slice(start, end < 0 ? undefined : end);
  return [...strip.matchAll(/<a\s[^>]*\bhref="([^"]*)"/g)].map((m) => m[1]);
};

const bad = [];
const landing = read("index.html");
const strip = landing === null ? null : stripHrefs(landing);
if (landing === null) bad.push("out/index.html is missing: run `next build` first");
else if (strip === null) bad.push("the built landing page has no standards strip");

// What is checked: each entry, then each strip link no entry names.
const links = standards.map((s) => ({ name: s.name, href: s.href, entry: true }));
for (const href of new Set(strip ?? [])) {
  if (!links.some((l) => l.href === href)) links.push({ name: "strip link", href, entry: false });
}

// The links with at least one failure: a link can fail more than one check,
// and the summary counts links, not messages.
const failing = new Set();
for (const l of links) {
  const fail = (why) => {
    bad.push(`${l.name} → ${l.href}: ${why}`);
    failing.add(l);
  };
  if (l.entry && strip !== null && !strip.includes(l.href)) {
    fail("the built landing page's strip does not link it");
  }
  const m = /^\/docs\/([a-z0-9-]+)\/(?:#([A-Za-z0-9_-]+))?$/.exec(l.href ?? "");
  if (!m) {
    fail("not a /docs/<slug>/ or /docs/<slug>/#<heading> link");
    continue;
  }
  const [, slug, anchor] = m;
  const doc = DOCS.find((d) => d.slug === slug);
  if (!doc || doc.href || !doc.file) {
    fail(`no docs page with the slug "${slug}" in lib/docs-registry.js`);
    continue;
  }
  const page = read(path.join("docs", slug, "index.html"));
  if (page === null) {
    fail(`out/docs/${slug}/index.html was not built`);
    continue;
  }
  if (anchor && !headingIds(page).has(anchor)) {
    fail(`${doc.file} has no heading whose id is "${anchor}"`);
  }
}

if (bad.length) {
  for (const b of bad) console.error(`check-standards: ${b}`);
  if (failing.size) {
    console.error(`check-standards: ${failing.size} of ${links.length} strip links do not resolve`);
  }
  process.exit(1);
}
console.log(`check-standards: ${links.length} strip links resolve`);
