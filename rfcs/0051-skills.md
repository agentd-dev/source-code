# RFC 0051: Skills — the MCP Skills extension and SKILL.md folders

**Status:** Accepted 2026-10-19, as track S of RFC 0048 (§5.2, §6 Q2); implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Builds on:** RFC 0028 §7 (the skills catalogue, which this replaces), RFC 0050 (an MCP extension on 2026-07-28 connections), RFC 0042 (what a served document may set), ADR 0001 (MCP extensions: the client side only)
**Standards:** the MCP Skills extension `io.modelcontextprotocol/skills`, SEP-2640 (Final), on the MCP base revision 2026-07-28. Pinned text: [ext-skills `specification/stable/skills.mdx` at `c1aa87e4c3`](https://github.com/modelcontextprotocol/ext-skills/blob/c1aa87e4c381c82b99aa37471d0eae4ab50f9985/specification/stable/skills.mdx) (2026-10-16), cited as [S]. Agent Skills, the `SKILL.md` format: [agentskills `docs/specification.mdx` at `69ef37e942`](https://github.com/agentskills/agentskills/blob/69ef37e9424c0a7ea9dd2293b559e43ec8176379/docs/specification.mdx) (unversioned), cited as [AS], and its non-normative [integration guide](https://github.com/agentskills/agentskills/blob/69ef37e9424c0a7ea9dd2293b559e43ec8176379/docs/client-implementation/adding-skills-support.mdx), cited as [G].

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

A skill is a folder of files, at least a `SKILL.md`, whose frontmatter names
and describes it and whose body tells an agent how to do one kind of work
[AS]. agentd had read skills from three places of its own invention: a
server's prompts, resources whose URI began `skill://` or whose MIME type was
`text/x-skill+markdown`, and loose Markdown files. None of them is a
standard. The user's answer to RFC 0048 Q2 was *"we should not treat prompts
as skills"*.

Skills now come from two places only:

- **An MCP server's Skills extension** [S], on a 2026-07-28 connection whose
  server declares it. agentd is its client (ADR 0001): it lists, gets, reads
  and verifies; it serves no skills.
- **`SKILL.md` folders on disk** [AS]: `<dir>/<name>/SKILL.md`, held to the
  Agent Skills rules.

The instruction document's `:::!skill` directive is unchanged.

## 2. The client of the Skills extension

| Duty [S] | agentd |
|---|---|
| Call `skills/*` only after the server declares the extension | A `skills.sources` server is asked only on a 2026-07-28 connection whose `server/discover` capabilities carry `extensions["io.modelcontextprotocol/skills"]`. Any other source is logged `skills.source.unavailable` with the reason, and nothing is sent. agentd declares the extension in `clientCapabilities.extensions` on its own `skills/*` and `resources/directory/read` requests, as the base revision's extension negotiation describes; [S] requires no client declaration, so a server may ignore it. |
| `skills/list`, paginated | Every page, following `nextCursor`, at most 64 pages, each request bounded by 30 s. An entry is checked on its own: one invalid entry is declined and the rest of the page is kept. |
| `skills/get {uri}` | Used to load a skill that no listing named, and to refresh an entry after a mismatch. `-32602` means the server does not serve that URI: an unknown skill. |
| An empty or partial listing is not proof of absence | `@skill:<server>/<path>` that names no listed skill of a source is asked of the server with `skills/get skill://<path>/SKILL.md`, and `skills.load {server, uri}` gets any URI. |
| `resources/directory/read` only with `directoryRead: true` | Sent only to a server that declares it, only to list the files of a `dynamic` skill an operator allows (D3): its root, then each subdirectory (`mimeType: inode/directory`) it lists in turn, up to 64 files and as many reads. A directory is never listed as a file, nor a child outside the skill. |
| Verify each file's `digest` and `size`; never use content that mismatches; refresh with `skills/get` | Every file read (`SKILL.md` and supporting files) is checked against its entry's `size` and `sha256:` digest over the raw bytes (a text content's UTF-8, a blob's decoded base64). On a mismatch the content is dropped and the entry is fetched again with `skills/get`, once: a changed `SKILL.md` digest is read again under the new entry; the same digest, or a second mismatch, declines the skill. |
| Compare the frontmatter field by field | `SKILL.md`'s frontmatter, parsed to a JSON object, must equal the entry's `frontmatter`: the same keys, each with an equal value. Any difference declines the skill, naming the first field that differs. |
| Refuse an entry whose `resources` is missing or invalid | Refused: `resources` absent or neither an array nor `"dynamic"`; a resource without a string `uri`, a `sha256:` and 64 lowercase hex digits, or a whole-number `size`; a duplicate `uri`; no resource for the skill's own `SKILL.md`; a file outside the skill's directory; a `uri` (the entry's or a file's) holding a character an RFC 3986 URI cannot carry unencoded — a space, a line break, a quote, an angle bracket; a `uri` that does not end in `/SKILL.md`; a last path segment that is not `frontmatter.name`; a `name` or `description` the Agent Skills rules refuse (D5). |
| `"dynamic"` is unverifiable | Declined unless the source allows it: `skills.sources[].dynamic: allow` (D3). |
| Accept at least 512 files and 16 MiB per skill | Accepted up to exactly 512 resources and 16 MiB (16,777,216 bytes) summed `size`; a skill over either is declined from its entry alone, saying which limit. One file may be the whole 16 MiB: a `resources/read` answer is read up to 24 MiB, room for 16 MiB as a base64 blob in its envelope, where every other MCP answer stays at 8 MiB. |
| Tell the user when a skill is declined | `skills.declined` (warn) names the skill and the reason, and whoever referenced it is told (D3). |
| Identify a skill by (server, `uri`) everywhere | The catalogue, the body cache, a context's loaded set and every `skills.*` event carry the configured server name and the `uri` (D2). The server's self-reported name is never used. |
| Never resolve the `skill://` authority through DNS | A skill URI is only ever sent back to the server that served it, as a `uri` parameter. |
| `resources/read` alone never activates a skill | Only a reference, a step's `skills:` or `skills.load` loads one. An `mcp.resource` step that reads a `SKILL.md` gets a plain resource. |
| Lazy retrieval | Discovery reads the listing only. `SKILL.md` is read when the skill is loaded, a supporting file when `skills.read` asks for it. |

The wire is agentd's own: rmcp 3.5 does not model the extension, so
`agentd-mcp` sends `skills/list`, `skills/get` and `resources/directory/read`
through `McpClient::call_raw`, as MCP Events' requests go, and reads files
with the SDK's `resources/read`. `agentd-mcp::skills` holds the wire view and
the entry checks that need no bytes; the digests are checked in agentd,
which has SHA-256.

## 3. Design decisions

**D1. Where skills come from.** From the Skills extension of a server named
under `skills.sources`, from `SKILL.md` folders (D5), and from the
instruction's `:::!skill` directive. A server's prompts are never skills, and
a resource is never a skill because of its scheme or MIME type ([S]: "A host
MUST NOT conclude that a resource is a skill merely because its URI carries
a particular scheme"). Prompts stay what they are: an `mcp.resource` step
reads one with `prompt:`. `skills.sources[].discover` is gone: it is
refused as any unknown key is, with no message of its own.

**D2. Identity and references.** A server skill is (server, `uri`): the
configured server name and the `uri` of its `SKILL.md`. Its *path* is the
`uri` without its scheme and without `/SKILL.md` (`skill://acme/billing/refunds/SKILL.md`
→ `acme/billing/refunds`). A local skill is the path of its `SKILL.md`; an
inline skill is its name in the instruction. agentd's own skills — inline,
project and user (D5) — share one origin, named `local` in a reference; a
`skills.sources` server named `local` is refused at load, so `local/…`
never names a server's skill.

A reference (`@skill:<ref>` in the instruction, a message or a step's
`skills:`, or `skills.load {name}`) is:

- `<name>` — the skills of that name in every origin; one is that skill;
- `<origin>/<name>` — the skills of that name in one origin (a source's
  server name, or `local`);
- `<origin>/<path>` — the server skill at that path.

More than one match is an error listing every candidate by its full form,
`<origin>/<path>`; a server never wins a name by being first, and a local
skill never silently hides a server's ([S], Names). Within `local` the
operator's skills have one name each: the instruction's over the project's
over the user's, the shadowed one logged `skills.collision`. The catalogue
the model sees names each skill by the shortest reference that is not
ambiguous, counted in one pass over the catalogue, so a listing of
thousands costs what reading it costs. A context names each skill it
loaded by the reference it loaded it under, which a later skill of the same
name does not change; `skills.read` and `skills.unload` find a loaded skill
by what the catalogue resolves the name to now, or else by that reference.

**D3. Verification and declines.** As §2. A skill is declined at discovery
when its entry is invalid, over a limit, `dynamic` without `dynamic: allow`,
or its frontmatter breaks the Agent Skills rules; at load when a file's
bytes or the frontmatter do not match. `skills.sources[].dynamic: allow`
loads a `dynamic` skill unverified: its frontmatter is still compared with
the entry, what is read of it counts toward 16 MiB, its files are read only
under its own directory from its own server, and with `directoryRead: true`
its directory and the subdirectories it lists are read when it loads. A
decline is a `skills.declined` event and is told to whoever asked: the
instruction's reference as a root note, a message's as a note in its
conversation, a step's as a line in the step's prompt beside its log line,
and a `skills.load` call as its tool error. A reference to a skill discovery
declined is told that decline, not that the skill is unknown.

**D4. Trust.** A server skill's body and files are untrusted input:

- In the model's context its body sits inside a block that names the server
  and the `uri` and says the text is that server's, not the operator's, and
  grants nothing; the catalogue marks it with its server. It is never
  rendered as a local skill, and nothing the server wrote leaves its block:
  the catalogue gives each skill one line (a line break or other control
  character in a description is a space), the header and the
  `<skill_content>` tag carry the server and `uri` on one line with quotes
  and angle brackets escaped, a `<skill_content` or `</skill_content` in the
  body, in any case, is made inert, and a line told to whoever asked about
  a decline or a failure is one line.
- Each server under `skills.sources` adds `untrusted_input` to the root
  grant's trifecta fold beside the tags it carries, so a configuration
  whose skills come from a server tagged only `sensitive` and `egress` is
  refused as the lethal trifecta unless `security.allow_trifecta`.
- `allowed-tools`, and every frontmatter field beyond `name`,
  `description`, `license`, `compatibility` and `metadata`, are ignored
  for every skill: a skill never grants a tool, a server or a permission.
- `scripts/` are never run: agentd runs no local code. A build with the
  `exec` feature refuses `security.exec.enabled` together with
  `skills.sources` at load, since nothing would then stop a server's skill
  from directing the model to run a command, and agentd has no per-skill
  approval to ask for.
- A supporting file (`references/`, `assets/`, anything listed) is read on
  demand with `skills.read {name, path}`, only for a skill loaded in the
  caller's context, only from that skill's own server, only a `uri` its
  held entry lists (an unlisted file is a verification failure), and
  verified as `SKILL.md` is. A file that no longer matches fetches the
  entry again; if it changed, the read says the skill changed and must be
  loaded again, and the context keeps the entry it loaded under until then.
- A nested `SKILL.md` is a file like any other: `skills.read` returns its
  text, and it loads as a skill only through its own entry.
- Server skill content is cached in memory only, keyed by (identity, hash),
  never written to disk, so it never sits in a discovery path. The hash
  binds the entry's listed files and digests, so a body loaded under an
  entry that has not changed is served again without a read.
- A reload keeps a context's loaded body only of a skill the new catalogue
  holds. A server's skill no listing named (got with `skills/get`) is held
  again when its source is still configured, admits its entry, and did not
  decline its `uri` this time. Every other server skill whose source the
  reload removed, or whose source no longer admits it (`filter`, `dynamic`,
  a decline), leaves the cache and every context's loaded set, logged
  `skills.unloaded` ([S]: remove a server's cached skill content when the
  server is removed); `skills.read` never reads from a server that is not
  a source.
- agentd keeps no per-skill approval. The operator's `skills.sources` entry
  is the approval of a server's skills ([S]: "Explicit user policy governs
  whether a skill is loaded at all").

**D5. Local skills.** `SKILL.md` folders are held to [AS]:

- A skill is a directory `<dir>/<name>/` holding a file named exactly
  `SKILL.md`, which begins with YAML frontmatter between `---` lines.
- `name`: 1–64 characters of `a-z`, `0-9` and `-`, neither starting nor
  ending with `-`, without `--`, equal to its directory's name.
- `description`: required, 1–1024 characters, not only whitespace.
- `license` (a string), `compatibility` (1–500 characters), `metadata` (a
  map of string keys to string values) and `allowed-tools` (a string,
  ignored) are optional. Other fields are kept and ignored, as [S] passes
  them through; the reference validator `skills-ref` refuses them, which
  would refuse skills written for other clients.
- A skill that breaks a rule is refused at load — the start exits 2 and a
  reload is refused — naming the file and the rule. A loose file in the
  folder (`README.md`, a `<name>.md`) is not a skill and is not read [G].

Discovery maps the guide's scopes onto agentd's configuration [G, Where to
scan]:

| [G] scope | agentd |
|---|---|
| user | `skills.dir`; when it is unset, `skills/` beside the configuration file once it holds a `<name>/SKILL.md` |
| project | `.agents/skills/` beside a loaded configuration file — the cross-client convention |

A project skill overrides a user skill of the same name, logged
`skills.collision`. Both are read only from the operator's configuration:
the project scope only beside a configuration file agentd loaded (never the
working directory alone, never the home directory), which is the trust
check [G] asks for.

**D6. Progressive disclosure.** The catalogue (each skill's reference,
description and, for a server's, its server) is in every system prompt. A
body enters a context when a reference or `skills.load` loads it, with the
skill's file list beside it. Files enter only through `skills.read`.
`skills.max_loaded` (8) caps a context's loaded skills and
`skills.max_bytes` (32768) the text a body or a file puts into context,
after the full bytes were verified; [S]'s 512 files and 16 MiB bound a
server skill itself.

**D7. Telemetry.** §5. A line names a server skill by `server` and `uri`, a
local one by `path`, an inline one by `origin: instruction` and `name`.

## 4. Configuration

```yaml
skills:
  sources:
    - server: playbooks       # a declared MCP server whose Skills extension is read
      filter: "deploy-*"      # optional: one name, or a prefix ending in *
      dynamic: refuse         # refuse (default) | allow
  dir: ./skills               # user scope; `skills/` beside the config by default
  reference_prefix: "@skill:"
  max_loaded: 8
  max_bytes: 32768
```

`.agents/skills/` has no key: it is a folder beside the configuration file.
Everything is reloadable; a reload lists the sources again and re-reads both
folders, and `lifecycle.watch_config` watches the folders and each
`SKILL.md`. `skills.sources` and `skills.dir` stay operator-only (RFC 0042).

## 5. Telemetry

| Event | Level | Fields |
|---|---|---|
| `skills.discovered` | info | `server` (a source, with `truncated`) or `scope` and `dir` (a folder) or `origin: instruction`; `count`; `skills`: `[{name, uri}]`, `[{name, path}]` or `[{name}]` |
| `skills.declined` | warn | `server` and `uri`, or `path`; `reason`; `file` (a supporting file's `uri`); `ctx`, or `run` and `step`, when a reference asked |
| `skills.loaded` | info | `ctx`; `skill` (the reference); `server` and `uri`, or `path`, or `origin`; `hash` (12 hex digits); `verified` |
| `skills.unknown` | warn | `skill`, `err` (unknown, or ambiguous with its `candidates`); `ctx`, or `run` and `step` |
| `skills.collision` | warn | `name`; `kept` and `shadowed` (an operator's skill over another), or `candidates` (origins that share the name) |
| `skills.load.refused` | warn | `ctx`, `skill`, `err` (`skills.max_loaded`) |
| `skills.load.failed` | warn | `skill`, `server`, `uri`, `err` (the server not connected, or a request failed); `ctx`, or `run` and `step` |
| `skills.source.unavailable` | warn | `server`, `reason` |
| `skills.unloaded` | info | `ctx`, `skill`; `server` and `uri`, or `path`, or `origin`; `reason` (a reload's catalogue no longer holds it) |

Each `skills/*`, `resources/directory/read` and skill `resources/read` is a
client span of the unit that asked, as other MCP requests are.

## 6. Security

- Skill content is untrusted model input and is labelled with its origin
  (D4); a digest match is consistency, not trust ([S], Digests Are Not a
  Trust Anchor): the same server supplies both.
- A server's skill reaches nothing beyond its own server: `skills.read` is
  bound to the skill's origin and to its held entry.
- No skill grants a tool or runs code; the `exec` build refuses to run
  beside server skills.
- Name collisions never resolve silently (D2).

## 7. Exit criteria

- Unit tests: the entry checks (each refusal, the limits at and past 512
  files and 16 MiB), [S]'s worked example's digests and sizes, the
  frontmatter comparison, every Agent Skills rule for a local `SKILL.md`,
  reference resolution (unique, ambiguous, qualified, nested paths, the
  `local` origin, project over user, thousands of skills at once), the
  origin-tagged rendering and what a server's text cannot do to it, the
  bodies a reload keeps, a `resources/read` answer past 8 MiB.
- End to end against agentd's mock MCP server, extended with the Skills
  extension: `skills/list` over several pages, a nested skill path, an
  invalid entry declined, a tampered file and a tampered `SKILL.md`
  declined after a `skills/get` refresh, a frontmatter mismatch, a
  `dynamic` skill declined and then allowed, a skill reached only through
  `skills/get`, two servers serving one name, a server that does not
  declare the extension never asked, prompts and `skill://` resources not
  skills, `skills.read` through the skill's own server, a file past 8 MiB
  read whole, a `dynamic` skill's subdirectory listed, a context naming and
  reading a skill by the reference it loaded, a reload that removes a
  source unloading its skills, `.agents/skills/` over `skills.dir`, and a
  broken local skill refused at load.
- Conformance family `mcp-skills`: a listed skill loaded once its digest
  matches, one whose bytes differ declined after a refresh, a prompt that
  is no skill.
- Docs: connect-mcp, mcp-reference, interoperability, configuration,
  telemetry-reference, security; the strip lists MCP Skills (SEP-2640) and
  Agent Skills (SKILL.md).
