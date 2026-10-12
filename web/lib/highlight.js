import { createElement } from "react";
import rehypeHighlight from "rehype-highlight";
import { common, createLowlight } from "lowlight";

/**
 * Syntax highlighting for fenced code, at BUILD time.
 *
 * Both markdown renderers (docs and the A2A extension specs) run in server
 * components, so the grammars run during static rendering and the browser gets
 * plain `<span class="hljs-…">` HTML — no highlight.js ships to the client.
 * The colours live in globals.css (`--syn-*`).
 *
 * Only the grammars the docs actually use are registered. A fence in any other
 * language stays plain text (rehype-highlight records a warning, the build goes
 * on). `mermaid` and `tui` must stay plain STRINGS: the docs page's `Pre` swaps
 * them for a diagram and a captured terminal frame, and reads their code as text.
 *
 * `markdown` fences stay plain too. In these docs they hold instruction
 * documents, whose bodies are YAML directives, and highlight.js's markdown
 * grammar misreads them: a snake_case `_` opens emphasis across lines, a
 * directive's indented YAML becomes an indented code block, and front matter's
 * closing `---` turns the line above into a heading.
 */
const HIGHLIGHT = {
  languages: {
    yaml: common.yaml,
    bash: common.bash,
    json: common.json,
    rust: common.rust,
    javascript: common.javascript,
    ini: common.ini,
  },
  aliases: {
    yaml: ["yml"],
    bash: ["sh", "shell", "console"],
    json: ["jsonc"],
    javascript: ["js"],
    ini: ["toml"],
  },
  detect: false,
  plainText: ["text", "mermaid", "tui", "markdown", "md"],
};

const REGISTERED = new Set([
  ...Object.keys(HIGHLIGHT.languages),
  ...Object.values(HIGHLIGHT.aliases).flat(),
]);

/**
 * Mark every `{{ … }}` in highlighted code as a template expression.
 *
 * highlight.js's YAML grammar already does this inside quoted strings, but not
 * inside block scalars (`instruction: |`), and other grammars not at all — yet
 * `{{secret:NAME}}` and `{{ steps.x.output }}` are agentd's syntax wherever
 * they appear (a YAML prompt, a shell flag, a JSON body). One pass after the
 * highlighter makes them read the same in every highlighted fence; a comment
 * keeps its own colour.
 */
function rehypeTemplateExpressions() {
  const EXPR = /\{\{.*?\}\}/g;
  const classes = (n) => n.properties?.className || [];
  const isTemplate = (n) => classes(n).includes("hljs-template-variable");
  const isComment = (n) => classes(n).includes("hljs-comment");
  // rehype-highlight tags a fence `hljs` before it learns the language is not
  // registered, so the language class is what says the fence was highlighted.
  const highlighted = (n) => classes(n).some((c) => REGISTERED.has(c.replace(/^language-/, "")));
  const mark = (value) => ({
    type: "element",
    tagName: "span",
    properties: { className: ["hljs-template-variable"] },
    children: [{ type: "text", value }],
  });
  const split = (parent) => {
    const out = [];
    for (const node of parent.children || []) {
      if (node.type === "element") {
        if (!isTemplate(node) && !isComment(node)) split(node);
        out.push(node);
        continue;
      }
      if (node.type !== "text" || !node.value.includes("{{")) {
        out.push(node);
        continue;
      }
      let last = 0;
      for (const m of node.value.matchAll(EXPR)) {
        if (m.index > last) out.push({ type: "text", value: node.value.slice(last, m.index) });
        out.push(mark(m[0]));
        last = m.index + m[0].length;
      }
      if (last < node.value.length) out.push({ type: "text", value: node.value.slice(last) });
    }
    parent.children = out;
  };
  const walk = (node) => {
    if (node.tagName === "code" && highlighted(node)) {
      split(node);
      return;
    }
    for (const child of node.children || []) walk(child);
  };
  return (tree) => walk(tree);
}

/** The rehype plugins every markdown renderer on the site runs for code. */
export const codePlugins = [[rehypeHighlight, HIGHLIGHT], rehypeTemplateExpressions];

const lowlight = createLowlight(HIGHLIGHT.languages);
lowlight.registerAlias(HIGHLIGHT.aliases);

/**
 * Highlight one sample that is not markdown (the landing page's terminals):
 * the same grammars and the same `hljs-*` spans, as React nodes, at build time.
 */
export function highlight(code, lang) {
  const node = (n, key) =>
    n.type === "text"
      ? n.value
      : createElement(
          "span",
          { key, className: n.properties.className.join(" ") },
          n.children.map(node),
        );
  return lowlight.highlight(lang, code).children.map(node);
}
