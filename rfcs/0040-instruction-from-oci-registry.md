# RFC 0040: Instruction documents from an OCI artifact registry

**Status:** Implemented (feature `oci`, default-off)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-09-05
**Extends:** RFC 0034 / RFC 0039 (instruction documents) — this adds a source, not a format.
**Depends on:** RFC 0031 (endpoint authentication — the auth axes are reused verbatim), the §7 signing + `instruction_sources` surface (Instruction Specification, shipped v1.8.0).

---

**Normative home:** the **Instruction Specification**
(https://github.com/instruction-md/specification). agentd is the reference
runtime; this RFC is its design rationale for a new *transport* of an
instruction document. The document format, the trust ladder, signing and the
delivery pipeline are unchanged — only where the bytes come from is new.

## 1. Summary

agentd already loads `agent.instruction` from three places: an inline string, a
local file (`--instruction-file`), and a resource URI (an `https://` fetch or an
MCP `resource`). This RFC adds a fourth: an **OCI artifact** in any
OCI-compliant registry — GHCR, Amazon ECR, Docker Hub, Harbor, Artifactory,
Azure ACR, Google AR, Quay.

An instruction document is pushed to a registry as a first-class **OCI
artifact** (image-spec 1.1) with a declared `artifactType`; agentd pulls it by
reference (`oci://registry/repo:tag` or `…@sha256:…`), verifies the blob digest,
and loads the plaintext exactly as if it had been read from a file. Nothing
about the document changes — a registry is a *distribution channel*, and the
whole point is to reuse one every team already runs.

**Why a registry rather than a bespoke channel.** A document that defines an
agent is an artifact with the same lifecycle needs as a container image:
immutable content-addressing (`@sha256:`), human tags (`:v3`, `:stable`),
RBAC, replication and geo-distribution, retention/GC, mirroring for air-gapped
sites, pull-through caches, and registry-native signing (cosign / the OCI
Referrers API). Reimplementing any of that would be worse than the thing it
replaced. An instruction document is already "the whole agent in one file"
(RFC 0039); shipping that file the way images ship makes an agent deploy like
any other workload.

## 2. The artifact

An instruction artifact is an OCI image manifest (media type
`application/vnd.oci.image.manifest.v1+json`) with:

- `artifactType`: **`application/vnd.instruction.document.v1`** — so a registry
  and a scanner can tell instruction artifacts apart from images by type, and
  the Referrers API can attach signatures/SBOMs to them.
- exactly one **layer** descriptor of media type
  **`text/markdown; variant=instruction`** (the spec's own media type, §11) —
  the instruction document bytes, gzip-optional.
- a small **config** blob (`application/vnd.instruction.config.v1+json`)
  carrying non-secret metadata: the spec version, the document `id`, and the
  publisher. The config is advisory; the layer bytes are authoritative.

```console
# push (any OCI client works; oras is shown)
$ oras push ghcr.io/acme/support-agent:v3 \
    --artifact-type application/vnd.instruction.document.v1 \
    agent.md:'text/markdown; variant=instruction'
```

An artifact is a normal blob store entry: it is content-addressed, so
`ghcr.io/acme/support-agent@sha256:…` is immutable and `:v3` resolves to a
manifest digest that agentd records as the pin.

## 3. The source

The reference is a URI scheme usable anywhere an instruction URI is:

```yaml
agent:
  instruction: "oci://ghcr.io/acme/support-agent:v3"
  # or, pinned immutable:
  instruction: "oci://ghcr.io/acme/support-agent@sha256:3f0a…"
```

It is equally valid as an `instruction_sources[].uri` (§7.5) and as an
`::include{uri="oci://…"}` target (§5.2) — one transport for the whole
composition graph.

The pull happens at **config load** (`from_document`), not at runtime — the
document's machinery must fold into the config being built (workflows, MCP
servers, the trust ladder), which is impossible once loading is over;
URL-fetched workflow definitions set the precedent for a load-time dial. The
resolved text then flows through decryption (RFC 0041) and idoc extraction
exactly like an inline document, and an `InstructionOrigin` (uri + manifest
digest) rides into the runtime so `instruction.loaded` logs the version pin
and the §7.7 freshness watch re-pulls the original reference. A freshness
re-pull delivers the re-extracted cleaned text; machinery CHANGES apply on
reload/restart (the §5.5 quiesce doctrine), and a document that no longer
folds keeps the running text — refuse-and-keep.

## 4. The pull

The OCI **Distribution Spec v2** is plain HTTPS + JSON, so agentd's existing
hand-rolled HTTP client carries it with no new dependency:

1. **Auth.** `GET https://<registry>/v2/` → `401` with
   `WWW-Authenticate: Bearer realm=…,service=…,scope=…`. agentd runs the token
   exchange against `realm` and presents the returned bearer on subsequent
   calls. The credential feeding that exchange reuses **RFC 0031's auth axes
   verbatim**: static bearer, HTTP Basic, OAuth2 client-credentials, **AWS
   SigV4** for ECR's `GetAuthorizationToken`, the ambient **docker config**
   (`~/.docker/config.json` + credential helpers), and **IRSA/IMDS** for
   in-cluster identity. No registry-specific code beyond the token dance the
   spec already standardizes.
2. **Manifest.** `GET /v2/<repo>/manifests/<ref>` with
   `Accept: application/vnd.oci.image.manifest.v1+json, …`. A tag ref returns a
   manifest whose **digest** (the `Docker-Content-Digest` header, re-verified by
   hashing the body) is the resolved pin.
3. **Layer.** Select the layer whose media type is
   `text/markdown; variant=instruction` (fail closed if absent or ambiguous).
   `GET /v2/<repo>/blobs/<digest>` → the bytes; **verify** their SHA-256 equals
   the descriptor digest before use. gunzip if the layer is gzipped.

The result is a byte-string handed to the same code path a file read feeds.

## 5. Integrity, signing, and freshness

- **Content integrity is by construction.** A blob is addressed by its SHA-256;
  a manifest by its own. A `@sha256:` reference can never serve different bytes,
  and a `:tag` reference is resolved to a digest that is recorded and logged —
  the same pin the §7 attestation and the resolution manifest already speak.
- **Authorship** is orthogonal and reuses §7 unchanged: the layer bytes may
  carry the author signature in front matter (verified over the plaintext
  digest), and the registry may hold a **cosign** signature via the Referrers
  API (`GET /v2/<repo>/referrers/<digest>`) — an optional second check that the
  operator can require. `instruction_sources` pins the publisher and keys as
  today; the OCI digest is the content the author signature commits to.
- **Freshness / revocation** (§7.7) composes with the existing re-fetch reactor
  hook: a `:tag` source re-resolves on its `freshness` interval, and a **changed
  manifest digest is a new version** (re-pull the layer, re-verify, apply the
  §5.5 lifecycle). A `@sha256:` source is immutable, so its "re-fetch" is a
  reachability probe only. A registry unreachable past the deadline freezes new
  work while live runs drain, exactly as a stale https source does.

## 6. Security model

The pulled bytes are **operator-designated configuration** — the operator
pointed agentd at this reference, the same trust act as `--instruction-file`.
So directives execute (the RFC 0034 trust rule is unchanged: execution follows
*where the operator pointed*, never what the bytes say). Two registry-specific
guardrails:

1. **A registry is a remote party.** A mutable `:tag` from a registry the
   operator does not control is a supply-chain surface. For a document that
   declares `compute` or `infra` blocks, agentd SHOULD require either a
   `@sha256:` pin or a valid §7 / cosign signature — the same "signed to
   execute over the wire" posture the trust ladder already takes. This is
   policy, configured per `instruction_sources` entry, not a new mechanism.
2. **Decrypted-later, still untrusted-shaped.** Nothing here weakens the
   trifecta or the grants; an instruction from a registry is exactly as
   constrained as the same bytes from a file.

## 7. Dependency budget

Zero new crates. The OCI Distribution API is HTTP + JSON over the existing
client and `serde_json`; SHA-256 and the auth signatures are `ring` (already in
the tree via rustls / `aauth`). The whole surface sits behind a default-off
**`oci`** cargo feature that rides `tls`. There is deliberately no
`oci-distribution` / `oci-client` / `oras` crate — the distribution protocol is
small and stable, and pulling one in would trade the minimalism moat for a few
hundred lines we can own. (This is the same call `aauth`, `oauth` and `sign`
made: reuse what rustls already carries, hand-roll the protocol glue.)

## 8. Non-goals

- **Push.** agentd pulls; it never publishes an instruction artifact. Authoring
  and signing happen in the control plane / CI with `oras` + `cosign`.
- **Running the artifact as an image.** An instruction artifact is markdown, not
  a runnable image; `!image`/`!runtime` blocks inside it still name OCI *images*
  through their own families (RFC 0039).
- **A registry client library.** Only the read path the loader needs — manifest,
  blob, referrers, the token dance — is implemented.

## 9. Open questions

- Whether to require a signature (not just a digest pin) by default for any
  `oci://` source, or only when `compute`/`infra` is present.
- Referrers API fallback: registries at Distribution spec < 1.1 expose
  referrers via the tag-schema fallback; whether to implement the fallback or
  require a 1.1 registry for cosign verification.
- Caching: whether a pulled blob is cached by digest on disk (a pull-through
  cache the operator may already run makes this redundant).
