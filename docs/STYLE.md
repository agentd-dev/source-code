# Writing the agentd docs

The docs are agentd's product documentation. They serve six readers, and each
page is written for the ones it names:

| Reader | Wants | Starts at |
|---|---|---|
| **Evaluator** — a CTO, a team lead, a buyer | what agentd is, what it is good for, what it costs to adopt, how it compares | Product |
| **Newcomer** — a developer trying it | a first agent running, then a real one | Get started |
| **Builder** — writes instructions and workflows | how to do a task, with a sample that works | Guides |
| **Operator** — deploys and runs it | deployment shapes, signals, telemetry, failure modes | Guides, Reference |
| **Reviewer** — security, platform, compliance | the model, the guarantees, the limits | Concepts, Security |
| **Integrator** — embeds agentd, builds a client | the APIs, the wire, the extension points | Reference, Guides |

## Where a page goes

Every page is one of five kinds, and its section says which.

- **Product** pages explain *why*: the problem, the value, the fit, the
  trade-offs. No configuration keys beyond the one sample that shows the shape.
- **Get started** pages are tutorials: a path the reader follows end to end,
  that works the first time, with every command and every file shown in full.
- **Guides** answer *how do I …?* for one task. They state the goal, show the
  shortest complete configuration that does it, then the variations and the
  traps. They link to Reference for every key rather than restating it.
- **Concepts** explain *how it works and why it is built that way*. They carry
  the diagrams. They do not list keys.
- **Reference** pages are for lookup: complete, exact, ordered for scanning
  (tables), and they do not argue.

A page that tries to be two kinds becomes two pages.

## The shape of a page

1. **Title** — a noun phrase for Concepts and Reference ("The agent loop",
   "Configuration reference"), a task for Guides ("Connect a model provider"),
   an outcome for Get started ("Build a coding agent").
2. **Opening** — two to four sentences: what the page covers, who it is for,
   and what the reader can do after it. No history.
3. **Body** — H2 sections in the order the reader needs them. A section
   opens with its point, then the detail.
4. **Next steps** — two to four links: where the reader goes from here.

## Voice

- Address the reader as *you*. Present tense, active voice: "agentd retries the
  call", not "the call will be retried".
- Lead with the point. A paragraph's first sentence says what the paragraph
  proves.
- One idea per sentence. Prefer two short sentences to one with a semicolon.
- Plain words over coined ones. Define a term the first time a page uses it,
  or link it to the [glossary](glossary.md). Internal names — RFC numbers,
  module paths, unit ids — belong in Concepts and Reference, never in a
  page's opening.
- State limits as plainly as features. "agentd does not do X; use Y" is a
  feature of the docs.
- Describe the product as it is. No history: no removed or renamed keys, no
  "previously", "now", "no longer", "since vX", "was replaced by", no migration
  notes, no version comparisons. The CHANGELOG is the only place history lives.
- No marketing superlatives, no exclamation marks, no "simply" or "just".

## Samples

- Every configuration sample is **complete and valid**: it loads with
  `agentd --validate-config --config <file>`, or it is marked as a fragment
  with a comment on its first line (`# fragment: under intelligence:`).
- Secrets appear only as references: `"{{secret:OPENAI_API_KEY}}"`.
- Real endpoints are written in full: an OpenAI-compatible endpoint is
  `https://api.openai.com/v1/chat/completions` (a path is posted to as
  written), Anthropic `https://api.anthropic.com`.
- Use current model names, and the same small cast across pages: `smart`,
  `fast`, `deep`, `jev`.
- Show the output a reader should see when it matters (a log line, an exit
  code), in its own block.
- Commands are copy-pasteable: no prompt characters, one command per line.

## Diagrams

- Mermaid, in a ```` ```mermaid ```` block; the site renders it and GitHub does too.
- A diagram earns its place by showing a flow, a boundary or a lifecycle that
  prose needs a paragraph for. Label every edge; keep to about a dozen nodes.
- Concepts pages carry the architecture diagrams; Guides use one at most.

## Formatting

- Sentence-case headings. No numbered headings.
- Tables for anything a reader scans: keys, fields, options, comparisons.
- Callouts as a blockquote that opens with a bold label: `> **Note.**`,
  `> **Warning.**`, `> **Limit.**`.
- Links between pages are relative (`[Workflows](workflows.md)`); the site
  rewrites them. A page's basename is unique across `docs/`.
- No line longer than about 80 characters in prose, so diffs stay readable.

## Registering a page

A page appears on agentd.dev when `web/lib/docs.js` lists it: its `slug`,
`file`, `title`, `group`, `section` and a one-sentence `blurb` (the search
snippet). Keep a slug once published — links point at it.
