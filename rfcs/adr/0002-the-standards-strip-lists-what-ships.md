# ADR 0002: The standards strip lists what the release binaries speak

**Status:** Accepted, 2026-10-19 — it records how the strip the user asked for on 2026-10-18 is kept
**Context:** RFC 0048; the strip on agentd.dev (`web/lib/standards.json`)

## Context

agentd.dev shows the protocols and specifications agentd supports as a row of
names, the way a company shows the logos of those who trust it. A row like
that is read as a claim, and agentd ships some support as previews (a cargo
feature outside the release set) and some against drafts.

## Decision

- An entry names what the **release binaries** speak, with its version.
- What only a preview feature speaks is marked **PREVIEW**; what tracks an
  unfinished specification is marked **DRAFT**.
- Every entry links to the part of agentd's documentation that covers it, and
  the site build fails when a link or its anchor does not resolve
  (`web/tools/check-standards.mjs`).
- Something planned is not listed until it ships.
- agentd's own formats (the workflow language) are listed, marked as
  agentd's; so is the one runtime role a reader looks for by name, PID 1.

## Consequences

Adding support means adding its entry in the same change; removing support
removes the entry. RFC 0048 §2 is the first change under this rule.
