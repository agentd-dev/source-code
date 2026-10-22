# ADR 0003: No payment without a spend policy

**Status:** Accepted, 2026-10-22 — the user's answer to RFC 0048 Q3 (2026-10-19, *"both"*: x402 on EVM and Solana) accepted payments under it; [RFC 0049](../0049-paying-for-things.md) §8 states how each rule is met
**Context:** RFC 0048 §5.6; RFC 0049

## Context

x402 and the other agent payment protocols let an agent pay for a request
with no person present. x402 itself covers only replay protection and names
client-side budget management out of scope. An agent that can pay can be
talked into paying — by a prompt, a tool result or a Company's text.

## Decision

agentd pays only under a spend policy the operator declares, and only behind
a feature that is off by default:

- caps per run, per day and per payee, and an allow-list of payees and
  assets — a payment outside them is refused unsent;
- the User's approval above a threshold, through an addressed human gate no
  model or judge answers (as RFC 0047's operation approvals are);
- the signing key held by the supervisor alone, sealed at rest, never in a
  model's context, a child's payload or a log;
- every payment an audit record, tied to the request that asked for it;
- a payment whose outcome is unknown is never made a second time.

## Consequences

No payment protocol ships without all five. Each protocol's RFC states how it
meets them before any code is written.
