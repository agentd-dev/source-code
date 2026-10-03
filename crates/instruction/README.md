# agentd-instruction

The reference implementation of the
[Instruction Specification](https://github.com/instruction-md/specification),
as a Rust library. The package is `agentd-instruction` and the library is
`instruction_core`.

An instruction is one Markdown file that defines an agent: the prose that
instructs it and the machinery that equips it. This crate parses such a
document into a typed block tree, validates it against the specification's
own registry, delivers it byte for byte the way §3.5 says a model must read
it, accounts for that delivery in the §7.4 resolution manifest, reports the
Appendix C advisories, and (behind `sign`) verifies §7 author and delivery
signatures.

agentd is the first consumer. Its `config::idoc` module re-exports this
crate's `doc` module and adds agentd's own configuration folding on top. The
crate takes no runtime types from agentd, so any other reader, a registry or
a publish-time validator can link it.

## What it implements

- **Instruction Specification version 1, registry revision 1.1.** That
  covers every 1.1 proposal the specification adopts (S8–S20 and S22–S26),
  plus **S7** (the resolution manifest) and **S27** (end matter).
- **S21**, the authoring guide, is informative and needs no code. Writers
  start with upstream's
  [guide](https://github.com/instruction-md/specification/blob/345f27588f5982cbe530254b82aba4ed90a34292/guide/README.md)
  and
  [cheat sheet](https://github.com/instruction-md/specification/blob/345f27588f5982cbe530254b82aba4ed90a34292/guide/cheatsheet.md).
- The registry is not transcribed into Rust. `src/instruction.schema.json`
  is the specification's own JSON Schema, embedded as text and parsed at
  first use, so a
  kind, a form, a grant, a keyword or a label cannot drift from the
  specification.

## The vendored revision

Two artifacts are upstream's bytes, both from
`instruction-md/specification` at revision
**`345f27588f5982cbe530254b82aba4ed90a34292`**:

- `src/instruction.schema.json`, the registry;
- `tests/conformance/`, the whole conformance directory: `corpus/`,
  `refusals/`, `advisories/`, and the `README.md` and `LICENSE` beside them.

Two drift tests compare them with a checkout of the specification:

- `the_vendored_corpus_matches_upstream_when_present` (this crate's
  `tests/corpus.rs`) compares every file in both directions, byte for byte;
- `the_vendored_schema_matches_upstream_when_present` (agentd-cli's
  `tests/instruction_spec_corpus.rs`) compares the schema as parsed JSON.

Each reads the checkout named by `INSTRUCTION_SPEC_REPO`; a path there that
lacks the vendored files fails the test rather than skipping it. With the
variable unset, each tries a maintainer's default path
(`/root/instruction-md/specification`) and, finding nothing there, prints
`drift check skipped` and passes — so outside that machine, set the
variable. CI and `scripts/ci-gate.sh` do not leave it to a skip. Their
`spec drift (pinned)` step checks out the revision `ci.yml` pins, runs both
tests by name against it, and fails the run when either skipped or did not
pass. `release_matrix.rs` holds both gates to that.

To re-vendor, from a specification checkout `<spec>` at the new revision:

```console
$ rm -rf crates/instruction/tests/conformance
$ mkdir crates/instruction/tests/conformance
$ cp -r <spec>/conformance/. crates/instruction/tests/conformance/
$ cp <spec>/instruction.schema.json crates/instruction/src/instruction.schema.json
```

In the same commit, move the `ref:` of `ci.yml`'s
`instruction-md/specification` checkout to the new full sha, and update the
revision named here and in agentd-cli's
`tests/instruction-spec-corpus/UPSTREAM.md`. If the shape of the vendored
tree changed, update `NOTICE` (this crate's and the repository's). A pin
that moves without a re-vendor, or a re-vendor without the pin, turns the
drift step red.

## The 0.3.0 API

0.3.0 breaks the 0.2 API. It is not yet published.

- **`Refusal` carries `code`.** That is the Appendix B code, a
  `&'static str`. A refusal serialises as `{line, code, message}`, and the
  message no longer carries `line N:`; `Display` still prints `line N:
  message`. `From<String>` and `message_body()` are removed. `CODES` lists
  every code, `NON_CATALOGUE` the ones Appendix B has no row for, and
  `UNDETECTED` the Appendix B codes this crate never emits, each with its
  reason (below).
- **Errors are structured.** `doc::parse`, `fold`, `fold_with_params`,
  `fold_full`, `extract`, `extract_with_facts` and `check_grants` return or
  push `Vec<Refusal>`, where they used `Vec<String>`. `sign::verify`,
  `verify_author`, `verify_delivery`, `verify_document` and `admit_family`
  return `Result<_, Refusal>` with no line, coded by Appendix B §7 or
  `attestation`; their text is unchanged.
- **`doc::fold_full(doc, granted, params, facts, resolver)`.** The include
  depth and the cycle path are internal and left the signature.
- **`Manifest` is the typed S7 shape:**
  - `authored: Authored { digest, version: Option<String> }`;
  - `parameters: Vec<ParameterUse { name, source, value_digest }>`, the
    substituted parameters only, sorted;
  - `facts: Vec<Fact { key, value_digest }>`, the facts a variant compared,
    sorted;
  - `variants: Variants { kept, dropped }`, with counter ids `when#n`,
    `unless#n` and `otherwise#n`;
  - `includes: Vec<Include { target, digest }>`;
  - `limits: Limits { include_depth, include_bytes }`, the depth reached and
    the UTF-8 bytes inlined;
  - `unresolved: Vec<String>`, required;
  - `overridden: Vec<String>`, omitted when empty.

  The 0.2 untyped fields are gone, with no compatibility path: a manifest
  that omits a field §7.4 requires does not deserialise. `deliver()` returns
  `Delivery { text, manifest }` from a single walk, so the manifest accounts
  for exactly the text delivered.
- **The signed form.** `Manifest::signed_form()` is the manifest without
  `authored.version`, and `Manifest::canonical()` returns its RFC 8785 (JCS)
  bytes. No discriminator key is added; `sign::SPEC_CLAIM` stays
  `"instruction/1"`.
- **Digests in every build.** `digest`, `author_digest`,
  `strip_front_matter_signature` and `front_matter_id` are exported at the
  crate root and compiled without `sign`.
- **Advisories.** `advise(&Document) -> Vec<Advisory>`, with
  `Advisory { line, code, severity, message }`, `Severity { Warning, Info }`
  and the `ADVISORY_CODES` table.
- **`Document`** gains `end_matter` and `has_front_matter`; its `source`
  excludes end matter, and `raw` keeps it. `doc::Node` gains `Note` and
  `Inert`. `doc::split_end_matter(text) -> Result<SplitEndMatter, Refusal>`
  splits a whole document at its end matter without parsing it (the body
  before the opener, the parsed map, the opener's line).
- **`tree_json`** emits `frontMatter` only when the document has front
  matter and `endMatter` only when it has end matter. It never shows an inert
  block. A keyword block carries `name`, `not`, `if` and `because`.
- **The registry carries the 1.1 tables:** labels, negated labels, keyword
  flags, strengths, context keys, label styles, the reason keyword, reserved
  bare names, the sigil table, the wire floor and the revision. Per kind it
  carries `alias_of`, the acknowledgement trigger, the informative body
  schema, whether a name is required, and each attribute's `enum` and
  `pattern` (`Registry::attr_rule`). `doc::reserved_bare_names()` is new.
- **`doc::needs_delivery(text)` replaces `contains_blocks`.** It is true
  for anything delivery renders, removes or refuses: a block, a keyword,
  alert or reason line, an inline reference, a column-0 author note, front
  matter or end matter. No alias is kept.
- **`Extraction`** gains `overridden` and `unfound_overrides`.
  `InlineSkill.when_to_use` is filled from `trigger`, with `when` as its
  alias, and `InlineSkill.body` is the rendered skill prose, not the raw
  text.
- **Delivered bytes change** for documents that use the 1.1 constructs, and
  for some version-1 documents too. The repository's `CHANGELOG.md` lists
  each change.

## Features

- **Default:** parse, validate, deliver, the manifest, the digests and the
  advisories. `ring` is a normal dependency, so SHA-256 exists in every
  build. A manifest whose digests depended on a feature would give one
  delivery two manifests.
- **`sign`:** §7 JWS/Ed25519 verification and nothing else: `sign::verify`,
  `verify_author`, `verify_delivery`, `verify_document`, `admit_family`,
  `Claims` and `Verified`.

## Refusal codes

Every refusal carries a code from `CODES`. That list is the Appendix B code
column, in its order, followed by the codes in `NON_CATALOGUE`, which
Appendix B has no row for:

- `schema`: the second entry of the `refusals/non-integer-version` fixture
  pins it, from the TypeScript port's schema-validation path.
- `attestation`: the §7 verification conditions Appendix B gives no row.
  These include a JWS that does not parse or verify, a wrong `alg` or spec
  claim, an expired signature, and a broken author-to-manifest chain.
- `nested-machinery` and `nesting-depth`: this reader's own limits.
  Machinery folds from a document's top level only, and a block nested more
  than 32 deep is not read.

These are to be raised upstream. `UNDETECTED` names the Appendix B codes this
crate never emits, with the reason for each:

- `text-after-close-fence` and `body-forbidden` cannot be reached under the
  grammar;
- `include-unavailable` degrades to the not-available note and is not
  refused;
- `unsupported-isolation`, `update-dangling-reference` and
  `update-trifecta-widening` are conditions of the consuming runtime, not of
  a document;
- `unknown-attribute` is deferred.

A unit test holds `CODES` and `UNDETECTED` to the code sites in `src/`.
Another test holds `CODES` to Appendix B of a specification checkout.

## Running conformance

```console
$ cargo test -p agentd-instruction
$ cargo test -p agentd-instruction --features sign
```

The corpus runner compares every vendored artifact: the delivered text, the
block tree, `manifest.json` and `manifest.canonical.json` of each corpus
case, each refusal case's `refusals.json`, and each advisory case's
`advisories.json`. A file it does not know how to compare fails as an unknown
artifact. To print one artifact of a case:

```console
$ cargo run -p agentd-instruction --example dump -- <mode> <case>
```

`<mode>` is one of `tree`, `text`, `manifest`, `canonical`, `refusals` or
`advisories`. `<case>` is a case directory such as
`crates/instruction/tests/conformance/corpus/variants`, or a `doc.md` with an
optional `context.json`. Includes resolve as the runner resolves them, so
every mode except `tree` prints the fixture's bytes. `tree` prints the same
tree with its members in `serde_json`'s key order.

## Licence

The package's licence is the `license` field of its `Cargo.toml`, with the
licence texts it ships (`LICENSE-APACHE`, `LICENSE-MIT`); each source file
names its own in its SPDX header. 0.1.0 was published AGPL-3.0-only, and that
version keeps that licence.

The vendored material keeps its own terms. The schema is **CC BY 4.0** and
the conformance fixtures are **Apache-2.0**, with upstream's `LICENSE` copied
beside them. `NOTICE` gives the attribution both require.
