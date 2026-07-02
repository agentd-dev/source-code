# RFC 0020: A2A interoperability over vsock — agentd as a first-class agent in the mesh

> **⚠ SUPERSEDED — transport + wire (target-vision pivot 2026-07-02; A2A-conformance 2026-07-02).**
> The unix/vsock transport this RFC specifies is replaced by **HTTPS+SSE**
> (mTLS/bearer auth; loopback `http://` for dev), served DIRECTLY — there is no
> HTTP↔vsock gateway. Operator control is unified into the `a2a.*` admin method
> family. The A2A protocol binding is now the **A2A spec §9 JSON-RPC binding
> verbatim**: bare PascalCase method names (`SendMessage`, `GetTask`, … — the
> `a2a.`-prefixed spelling is still accepted on input), the `SendMessageResponse`
> `{task}` envelope, `returnImmediately` defaulting to blocking, `CancelTask` of a
> terminal task → `UnsupportedOperationError`, and no non-spec `final` flag
> (termination is terminal-state + stream close). The AUTHORITATIVE description is
> the `crates/agentd/src/mcp/a2a.rs` module doc; this RFC's method-naming/streaming
> prose is historical. See [`../docs/design/00-target-vision-pivot.md`](../docs/design/00-target-vision-pivot.md).

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentd rewrite — control-plane track (RFC 0014); extends the self-MCP surface (RFC 0005) and the vsock management transport (RFC 0015)

---

## 1. Problem / Context

[A2A](https://a2a-protocol.org) (Agent2Agent) is the open, Linux-Foundation-governed
standard for **agent↔agent** interoperability — the horizontal complement to MCP's
vertical **agent↔tools**. The industry consensus is now settled: *MCP for tools,
A2A for agents.* A2A's model — an **Agent Card** (capability discovery), a **Task**
with a lifecycle (`submitted → working → completed | failed | canceled | rejected`),
and methods `SendMessage` / `GetTask` / `CancelTask` / `SubscribeToTask` — is, almost
exactly, the surface agentd already exposes as MCP self-tools (`subagent.spawn/send/
status/cancel` + the `agent://run|subagent` resources, RFC 0005/0009). **agentd is
already task-shaped; A2A is the standardized wire for what it already does.**

The obstacle is transport. A2A's bindings are JSON-RPC 2.0 / gRPC / REST over
**HTTP**, with **SSE** streaming, **webhooks**, and **enforced auth** (OAuth2 / OIDC /
mTLS). agentd is the deliberate opposite (RFC 0011/0012/0014): blocking stdio/unix/
**vsock**, no HTTP server (deferred, RFC 0013), notify-then-read instead of SSE, **no
network auth** ("the transport is the boundary", RFC 0012 §3.8), and **stateless**
(distillate-only, RFC 0011). A naïve "agentd runs an A2A HTTP server" would import an
HTTP stack, SSE, OAuth, a webhook registry, and task persistence — a frontal assault
on the minimalism moat.

This RFC resolves the tension with the same boundary the whole control-plane track
uses (RFC 0014 §3: *primitives in agentd, the network surface in the gateway*):

> **agentd serves real A2A over vsock; an on-node gateway bridges HTTP↔vsock.**

Because agentd serves *real* A2A (not a bespoke surface), the gateway is a **dumb
transport bridge** — it re-envelopes A2A JSON-RPC frames between HTTP/SSE and vsock,
terminates TLS, and enforces auth — **not** a protocol translator. The heavy network
machinery (HTTP, SSE, OAuth, webhooks, durable task history) lives in the gateway,
exactly where network concerns belong; agentd carries none of it.

---

## 2. Decision

1. **A2A is served over vsock/unix, never HTTP-in-agentd.** A new `a2a` feature adds
   an A2A server profile to the existing self-MCP listener (RFC 0015 §3 — same
   blocking, thread-per-connection, JSON-RPC-2.0-codec machinery, RFC 0004). The
   default build and the cloud-native image set are unchanged; A2A is opt-in.

2. **The on-node gateway is a dumb HTTP↔vsock transport bridge + policy enforcement
   point (PEP).** It is the `agentctl` node-agent (RFC 0014 §2) — it terminates TLS,
   authenticates the cluster/cross-vendor A2A client (OAuth/OIDC/mTLS), re-frames
   JSON-RPC between HTTP/SSE and vsock, holds the **webhook registry** and the
   **durable task history** (`ListTasks`), and forwards trusted requests over vsock.
   agentd carries **zero** of that.

3. **The Agent Card is the capabilities manifest (RFC 0015 §5.2), projected.** One
   builder; the A2A `agent.json` is a re-serialization of the manifest into A2A's
   schema. No second source of truth.

4. **An A2A Task is an agent run / subagent handle.** `SendMessage` starts a run
   (the async-subagent machinery, RFC 0005/0009); `GetTask`/`SubscribeToTask` read
   `agent://run|subagent/{handle}`; `CancelTask` is `subagent.cancel`. The
   **TerminalStatus enum (RFC 0007 §3.4) maps to A2A Task states** (§5).

5. **Streaming is status-level over vsock; the gateway makes SSE.** agentd emits Task
   *status* transitions (and the final artifact = the distillate) as line-framed
   JSON-RPC notifications over vsock; it does **not** stream partial artifacts (the
   distillate-only invariant, RFC 0009 §8, holds). The gateway re-frames the vsock
   event stream into SSE for HTTP clients.

6. **The subagent decision (the three layers).** *Keep the supervised local subagent
   model; make A2A the external surface; add remote-A2A as a delegation backend.* See
   §3 — this is binding and is the answer to "do subagents become A2A?"

7. **This likely obviates RFC 0013's deferred "Streamable HTTP serving" for agentd.**
   The gateway does HTTP; agentd never needs an HTTP server. RFC 0013's item is
   reclassified as *the gateway's concern, not agentd's* (§8).

8. **MCP is unchanged and retained.** agentd stays an MCP **client** (consuming tools
   — that is what MCP is for) and keeps its MCP **self-serving** surface (RFC 0005) as
   a gated **compat** surface for MCP-ecosystem peers. A2A is the *standards-aligned*
   external agent surface; MCP-self-serving is not removed, just no longer the only
   way to drive agentd.

---

## 3. The three layers — what changes and what does not

"Subagents use MCP" conflates three surfaces. They decide differently:

| Layer | Today | Decision |
|---|---|---|
| **Internal control** — supervisor ↔ re-exec'd child over the private length-framed channel (ctrl/ready, ping/pong, spawn payload, distillate up) | a private control protocol (**not** MCP) | **Unchanged.** A2A has no process supervision, PDEATHSIG, kill ladder, or cgroup bounds — the very guarantees that are agentd's reason to exist (RFC 0003/0009). Routing in-process supervision through a network task protocol is wrong by construction. |
| **External agent surface** — a peer/orchestrator drives agentd | served self-MCP `subagent.*` tools (MCP reused for agent-control) | **A2A-over-vsock becomes primary**; MCP-self-serving becomes a gated **compat** surface. A2A is purpose-built for "drive a remote agent"; the manifest *is* an Agent Card; a run *is* a Task. |
| **Delegation targets** — a coordinator spins up sub-work | always a local re-exec'd subagent | **Add a remote-A2A-agent backend** beside the local subagent. Same abstraction (objective + scope + budget → distilled result); the model/coordinator picks the backend. agentd becomes an A2A **client**. Local subagents stay the default for tight, supervised, bounded sub-tasks. |

**Net:** the moat (supervised local subagents) is untouched; A2A is added where it is
purpose-built (the external surface + cross-mesh delegation). MCP keeps the
tool-client role and a compat self-surface.

---

## 4. Why this holds the minimalism moat

Serving A2A-over-vsock, who carries what:

| A2A requirement | Home |
|---|---|
| JSON-RPC 2.0 | **agentd** — has the codec (RFC 0004) |
| vsock transport | **agentd** — already serves it (RFC 0015 §3) |
| Agent Card | **agentd** — = the manifest (RFC 0015 §5.2) |
| Task lifecycle (`SendMessage`/`GetTask`/`CancelTask`) | **agentd** — the async-subagent/`agent://run\|subagent` machinery |
| HTTP server, SSE framing | **gateway** |
| OAuth/OIDC/mTLS auth | **gateway** (PEP) — agentd trusts the vsock peer (one trust domain, RFC 0012 §3.8) |
| Webhooks / push-notification registry | **gateway** |
| Durable task history / `ListTasks` | **gateway** (agentd serves only *live* tasks from its ephemeral registry) |

agentd gains an A2A method surface (feature-gated) over a transport, codec, and
manifest it already has, and carries **no** HTTP/SSE/OAuth/webhook/persistence code.
This is the **intelligence-sidecar pattern inverted** (RFC 0006): agentd dials
intelligence *out* over vsock behind a TLS-terminating sidecar; it serves A2A *in*
over vsock behind an HTTP-terminating gateway. The strongest posture it enables: a
pod with **no cluster network at all** — vsock-out for the model, vsock-in for
management + A2A.

---

## 5. A2A ↔ agentd mapping (normative)

| A2A | agentd | Owner |
|---|---|---|
| Agent Card (`/.well-known/agent.json`, served by the gateway) | capabilities manifest, projected | RFC 0015 |
| `SendMessage` / `SendStreamingMessage` | start a run (`subagent.spawn{async}` machinery) | RFC 0009 |
| `GetTask` / `SubscribeToTask` | read/subscribe `agent://run\|subagent/{handle}` | RFC 0005 |
| `CancelTask` | `subagent.cancel` → kill ladder | RFC 0003/0005 |
| multi-turn (`contextId`/`taskId`) | warm session (`subagent.send`) | RFC 0005 |
| `Artifact` (final) | the **distillate** | RFC 0009 §7 |
| Task states | TerminalStatus (RFC 0007 §3.4): `completed`→COMPLETED, `refused`→REJECTED, `cancelled`→CANCELED, `exhausted_*`/`deadline`/`stalled`/`loop_detected`/`crashed`→FAILED | RFC 0007 |
| `ListTasks`, history, push-notification config | **gateway-held** (durable, stateful) | RFC 0014 (gateway) |

Topology note: vsock is point-to-point guest↔host, so the gateway is **on-node** (the
node-agent DaemonSet); a cluster A2A service fronts the per-node gateways. The gateway
passes the authenticated client/tenant identity to agentd as **descriptive metadata**
(like the downward-API identity, RFC 0015 §6) — agentd labels/scopes by it but never
re-verifies it (the gateway already did).

---

## 6. Failure semantics & versioning

- **Auth handoff.** The gateway is the PEP: an unauthenticated/over-quota/forbidden
  client never reaches vsock. agentd trusts every vsock request absolutely (RFC 0012
  §3.8). A compromised gateway is a compromised node trust domain — the same blast
  radius as a compromised node-agent for management (RFC 0015 §7).
- **A2A version negotiation** is the gateway's job (`A2A-Version`, `VersionNotSupported`);
  agentd serves one A2A version, surfaced in the manifest/`surfaces` (RFC 0015 §5.2).
- **Stateless agentd, stateful gateway.** A pod reschedule loses live tasks; the
  gateway re-drives idempotently (RFC 0011 §6 RUN_ID) or surfaces FAILED. Durable
  history survives in the gateway, never in agentd.
- **Streaming subset.** Clients requesting partial-artifact streaming get status-level
  streaming + a single final artifact; the Agent Card advertises this honestly
  (`capabilities.streaming: true` meaning status streaming).

---

## 7. Non-goals (these stay in the gateway / agentctl)

- The HTTP server, SSE, webhook delivery, OAuth/OIDC/mTLS, TLS termination.
- Durable task history, `ListTasks` persistence, cross-pod task aggregation.
- A2A version negotiation, tenant isolation policy, rate-limiting, the public
  `/.well-known/agent.json` endpoint.
- Discovery/registries, the cluster A2A ingress/service.

agentd contributes the A2A-mappable primitives it already has (manifest, run/subagent
task handles, JSON-RPC, vsock) and the thin `a2a` method surface that binds them.

---

## 8. What this changes in the rest of the set

- **RFC 0013** — "Streamable HTTP serving" is reclassified: **not agentd's job; the
  gateway's**. agentd may never implement an HTTP server. (Alignment note added there.)
- **RFC 0005** — the self-MCP `subagent.*` surface is now the **compat** external
  agentd surface; A2A-over-vsock is the standards-aligned primary. (Alignment note.)
- **RFC 0009** — the subagent/TerminalStatus model is declared **A2A-Task-mappable**,
  and gains the **delegation-backend** axis (local subagent | remote A2A agent).
- **RFC 0014/0015** — A2A is part of the mesh story; the manifest is an Agent-Card
  projection; the node-agent is also the A2A gateway; the vsock transport carries A2A.
- **RFC 0006** — A2A-over-vsock-behind-a-gateway is the inverse of intelligence-over-
  vsock-behind-a-sidecar; the symmetry is noted.

The control-plane track core (operator management — provision/scale/observe/config/
intelligence, RFC 0014–0019) is **unaffected**: A2A is agent↔agent *work* interop,
orthogonal to the operator plane and complementary to MCP.

---

## 9. References

- RFC 0003 — supervision (the kill ladder / PDEATHSIG A2A cannot replace)
- RFC 0004 — JSON-RPC codec (reused for A2A)
- RFC 0005 — self-MCP surface (now the compat external surface)
- RFC 0006 — intelligence transport (the symmetric vsock-sidecar pattern)
- RFC 0007 §3.4 — TerminalStatus (maps to A2A Task states)
- RFC 0009 — subagent model (= A2A Task; gains the delegation-backend axis)
- RFC 0011 — statelessness / idempotency / exit codes
- RFC 0012 §3.8 — the trust-domain / no-network-auth posture (the gateway is the PEP)
- RFC 0013 — deferred HTTP serving (obviated for agentd by the gateway)
- RFC 0014 — control-plane umbrella (the node-agent gateway, the boundary)
- RFC 0015 — vsock serving + the capabilities manifest (= Agent Card)
- [A2A specification](https://a2a-protocol.org/latest/specification/)
