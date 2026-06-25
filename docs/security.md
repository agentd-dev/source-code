# Security

> Spec: [RFC 0012 — Security posture](../rfcs/0012-security-posture.md). Binding
> decisions: [`docs/design/00-architecture-assessment.md`](design/00-architecture-assessment.md).
> Build status / milestones: [`docs/design/PLAN.md`](design/PLAN.md).

agentd connects an LLM-driven loop to *arbitrary, operator-declared* MCP servers and an
optional `exec` capability. That is, by construction, the worst-case shape for the one
unsolved problem in agent security: **prompt injection** — and its acute form, Willison's
**lethal trifecta**, where a single agent simultaneously (1) reads untrusted content,
(2) holds sensitive data or tools, and (3) can communicate externally. Any agent holding all
three legs is a one-injected-prompt exfiltration tool.

Prompt injection is **not patchable**. A 95%-effective guardrail is a failure in security
terms. So agentd does not pretend to solve it with a classifier or a policy DSL. It contains
it structurally.

## The thesis: minimalism + structural isolation is the moat

agentd ships **no policy engine, no request signing, no auth, no RBAC** in core — not behind a
feature gate, not anywhere. This is a conscious reversal of the retired "governance is the
moat" design (a Rego-style DSL, ed25519 signing, JWT/x509). The reasoning:

- A policy engine living *inside* an injectable model loop is theatre. The model can be steered
  to emit policy-compliant-but-malicious actions, and the engine itself is pure binary weight
  and new attack surface.
- agentd's security is instead the **OS process tree**, the **granted MCP subset read as a
  trust budget**, and **distilled structured returns as an injection firewall**. These cost
  near-zero binary weight, which keeps them consistent with the minimalism bar that *is* the
  product moat.

Concretely, the posture has eight load-bearing parts:

1. The **outer boundary is the sandbox** — container / VM / microVM / enclave.
2. **Capability scoping = the granted MCP subset**, interpreted as a Rule-of-Two trust budget.
3. **Process isolation + distilled returns** form an injection firewall.
4. **All MCP server content is untrusted** — including tool descriptions (tool poisoning).
5. **SSRF defenses** live in the one hand-rolled HTTP client.
6. **`exec` is off by default** and gated.
7. **Self-MCP serving is stdio/unix only** in v1 (HTTP serving deferred).
8. **Secrets are env/flag only**, behind a `resolve()` front door, never logged.

---

## 1. The outer boundary is the sandbox

agentd is sandbox-*aware*, never sandbox-*providing*. It does **not** seccomp, chroot, or
namespace itself. Confinement, egress policy, filesystem scope, and aggregate resource limits
are the deployment's job:

- **Confinement / sandboxing** — container, VM, microVM, or enclave.
- **Egress network policy** — which hosts the whole pod may reach is a NetworkPolicy / firewall
  concern. The SSRF guards (§5) are a *second* line, not the only one. The recommended
  container shape terminates TLS at a sidecar, so most builds link no TLS at all.
- **Aggregate memory** — cgroups v2 (`memory.max`, `pids.max`, `cgroup.kill`). agentd enforces
  only the *token* ceiling and per-child `RLIMIT_AS`/CPU in-binary; aggregate subtree memory is
  a cgroup concern. agentd is cgroup-*aware*, never cgroup-*requiring*.

Run agentd as if the process itself could be compromised, because under a successful injection
it effectively is. The blast radius is whatever the surrounding sandbox permits.

---

## 2. Capability scoping = the granted MCP subset, as a trust budget

agentd ships **no tools of its own** except a gated `exec` and its self-MCP control tools
(`subagent.*`, resource read/subscribe). Every other capability comes from operator-declared
MCP servers. A subagent's capability set is exactly the MCP subset it was granted, and scope
**narrows monotonically** down the subagent tree (RFC 0009) — a child can never hold more than
its parent.

### Tool tags

The trust budget is built on three **operator-declared** tags per tool. They come from
operator config, *never* from server-supplied metadata (which is untrusted, §4):

| Tag | Meaning |
|-----|---------|
| `untrusted_input` | tool returns content from an uncontrolled source — web pages, inbound email, issue text, arbitrary files |
| `sensitive` | tool exposes private data or privileged systems — secrets store, internal DB, prod control plane |
| `egress` | tool can move data out of the trust boundary or change external state — HTTP POST, send mail, open PR, `exec` |

**Untagged tools default to `untrusted_input: true`** — the safe assumption is that any tool's
output may carry an injection; operators downgrade explicitly. The built-in self-MCP tools
carry fixed tags: `exec` ⇒ `egress` (and `sensitive` when not jailed); resource read/subscribe
inherit from the underlying server; `subagent.*` is untagged (it is the chokepoint, not a leaf
capability).

The tag set is a **budget, not an allow/deny rule**. It bounds what a single isolation unit
(one subagent process) may *simultaneously* hold.

> **Status (roadmap, M6).** Per-tool tagging via MCP server config and the Rule-of-Two check
> below land in milestone M6 (see PLAN.md). The tags JSON below and `--allow-trifecta` are the
> intended v1-target surface; they are **not** in the current CLI. The flags that exist today
> are listed in §8.

Intended config shape (tags attach by tool-name glob, longest-glob-wins, so one server can be
split into e.g. a `read_*` subset that is `sensitive` but not `egress`):

```json
{
  "mcp": {
    "web":   { "cmd": ["mcp-fetch"], "tags": { "*": ["untrusted_input"] } },
    "vault": { "cmd": ["mcp-vault"], "tags": { "*": ["sensitive"] } },
    "mail":  { "cmd": ["mcp-smtp"],  "tags": { "send_*": ["egress"] } }
  }
}
```

### The Rule-of-Two check at the spawn chokepoint

All scope grants flow through the supervisor-owned `subagent.spawn` — the single unforgeable
chokepoint (the supervisor mints depth; the child re-execs agentd's own `argv[0]`, never a
model-supplied path). Before re-exec, the supervisor ORs the tags across the granted tool set
and evaluates the budget:

- **Refuse** (default): a grant that hands one subagent all three trifecta legs is refused.
  Crucially this is **not a crash and not a JSON-RPC error** — `subagent.spawn` returns an MCP
  tool result with `isError: true`, which the parent's model sees as an *observation* and adapts
  to (RFC 0007):

  ```json
  {"isError": true,
   "content": [{"type": "text",
     "text": "refused: this grant gives one subagent all three lethal-trifecta legs
              (untrusted_input + sensitive + egress). Split into reader/actor subagents,
              or relaunch agentd with --allow-trifecta to override."}]}
  ```

- **Warn** (with `--allow-trifecta`): the spawn proceeds and the supervisor emits an auditable
  log event so the override is never silent:

  ```json
  {"level":"warn","event":"scope.trifecta_grant","agent_path":"root/reader",
   "agent_id":"a1b2","legs":3,"tools":["mcp-fetch.get","mcp-vault.read","mcp-smtp.send"]}
  ```

  `--allow-trifecta` is **process-global** and does **not** propagate into spawn payloads — a
  child cannot re-grant itself the override; it stays the supervisor's per-spawn decision.

- **Ok**: silent.

The check is **purely structural**. It never inspects content and never asks the model to judge
safety. Because scope narrows monotonically, a child can never widen its tag union beyond its
parent's, so the budget is enforced identically at every level of the tree.

The recommended pattern (encoded in the `subagent.spawn` tool description) is to split a
trifecta task into a **reader** (no sensitive, no egress) that returns a distilled summary, and
an **actor** (no untrusted input) that consumes it — which is exactly the firewall in §3.

---

## 3. Process isolation + distilled returns = the injection firewall

This is the load-bearing structural defense, and it falls out of the subagent result contract
for free. A child subagent returns a **distilled, structured value (~1–2k tokens) + terminal
status + usage** up the length-framed control channel. The parent appends the *distillate* —
**never** the child's raw transcript. Two security properties follow with zero extra mechanism:

1. **Content quarantine.** Raw untrusted bytes — a poisoned web page, a malicious tool
   description echoed back in a tool result — live only inside the reader subagent's context and
   are deleted when that process exits. The parent's context (which holds the sensitive/egress
   tools) never ingests them, so an injection in that content cannot author actions in the
   parent. This is CaMeL's trusted-planner / untrusted-data split, realized as **OS process
   isolation** rather than a taint-tracking interpreter.

2. **Bandwidth limiting.** A 1–2k-token distillate is a low-bandwidth channel. Exfiltrating a
   secret *through* the summary requires the reader to encode it — but the reader has no
   sensitive tools and therefore holds no secret to leak. With scopes split per §2, exfiltration
   is structurally, not statistically, prevented.

```
  untrusted source ──▶  READER subagent          ACTOR subagent  ──▶ egress
  (web/email/files)     tags: untrusted_input     tags: sensitive,egress
                        NO sensitive, NO egress   NO untrusted_input
                              │                        ▲
                              └── distilled summary ───┘
                                  (~1–2k tokens, no raw bytes cross the line)
```

**Defense-in-depth (recommended, not enforced in v1):** the parent specifies the child's
*output contract* as a constrained shape (enum/struct fields, not free prose), so injected
instructions in the child's input have no syntactic place to surface in the return. agentd does
not *enforce* schema-constrained returns in v1 (that needs provider strict-mode plumbing); the
firewall holds on isolation alone, and the constrained shape is a recommendation on top.

---

## 4. All MCP server content is untrusted — including tool descriptions

Every byte that originates from an MCP server is untrusted model input — **including the parts
the protocol presents as trusted metadata**: a tool's `name` / `description` / `inputSchema`,
its `annotations`, resource `description` / `mimeType`, prompt text, and of course tool results.
This is **tool poisoning** (OWASP ASI01): a malicious server ships a description that carries an
injection, or quietly mutates it after first connection (a "rug pull").

Concrete rules:

- **No auto-trust of server metadata.** Tool descriptions and annotations are passed to the
  model as the tool catalogue, but are **never** used to make a security decision. Tags come
  from operator config (§2), never from `annotations`. The `readOnlyHint` / `destructiveHint`
  annotations are treated as untrusted hints — surfaced for audit, never load-bearing.
- **Audit surface.** On `tools/list`, agentd logs each tool's
  `{server, name, description_hash, description_len}` at `info`
  (`event:"mcp.tool.listed"`). A description whose hash changes between connections logs
  `event:"mcp.tool.description_changed"` at `warn` — rug-pull / TOCTOU detection.
- **Launch commands are never model- or server-derived.** The set of MCP servers and their
  `argv` come *only* from operator config (`--mcp`), validated at startup (bad config → exit 2).
  The model cannot add a server, edit an `argv`, or make agentd spawn a process from a string it
  produced. `subagent.spawn` re-execs agentd's own `argv[0]`; `exec` runs an operator-allowed
  binary, never a server-named one.

> **Spawning a stdio MCP server means trusting that command as code at agentd's privilege.**
> Declaring `--mcp name=cmd` is an operator trust decision equivalent to running `cmd` yourself.
> Vet your servers the way you vet any dependency you execute.

- **stdio is the default transport.** A stdio MCP server can reach only agentd's pipes — not the
  network, not other processes — which is itself a confinement win over an HTTP server.

---

## 5. SSRF defenses in the hand-rolled HTTP client

agentd's single hand-rolled HTTP/1.1 + SSE client is the **only** outbound network primitive,
and therefore the only SSRF chokepoint. It carries the `https://` intelligence transport (and
any future HTTP-MCP). Guards apply **after DNS resolution and on every redirect hop**:

- **HTTPS in prod.** Plaintext `http://` targets are rejected by default; loopback dev may relax
  this.
- **Block private / loopback / link-local by default** — RFC-1918 (`10/8`, `172.16/12`,
  `192.168/16`), `127/8`, `169.254/16`, `0.0.0.0/8`, IPv6 `::1` / `fc00::/7` / `fe80::/10`, and
  v4-mapped-v6 forms. The `169.254/16` block specifically denies the `169.254.169.254`
  **cloud-metadata** SSRF; the v4-mapped and `0.0.0.0/8` cases close the usual bypasses.
- **DNS pinning / anti-rebinding.** Resolve once, vet the resolved IP(s), then connect to the
  *vetted* IP — not a fresh re-resolution. This closes the DNS-rebinding TOCTOU between the
  policy check and the connect.
- **Validate redirects.** Each `3xx` `Location` is parsed, re-vetted (scheme + resolved IP), and
  counted against a redirect cap. A cross-host or downgrade (`https`→`http`) redirect is
  **refused, not followed**, and surfaced as the request error.
- **CR/LF-injection-rejecting headers.** Header names/values containing `\r`, `\n`, or NUL are
  rejected at construction, so no string (including a model-produced one) can split a request or
  inject a header. Secret-bearing header values are resolved *after* this check and the resolved
  secret is itself CR/LF-validated.

These are a few tens of lines of checks, not a library — consistent with the no-`url`-crate /
no-ICU dependency stance.

> **Status (roadmap, M6).** The SSRF guards land in M6 (PLAN.md). The HTTPS/private-range policy
> knobs described in RFC 0012 (e.g. allowing localhost/plaintext for dev) are part of the
> v1-target client and are not yet exposed as CLI flags.

---

## 6. `exec` — off by default, gated, least-exposed

`exec` is the strongest egress leg and therefore the most dangerous trifecta member. Rules:

- **Off by default.** Absent `--enable-exec`, the `exec` self-tool is **not registered** in the
  self-MCP `tools/list` — the model never sees it, so it cannot be discovered or poisoned into
  existence. (Absent, not "present but erroring.")
- **Capability-checked at startup.** `--enable-exec` validates the allowed binary exists and is
  executable; a missing binary is a **config error → exit 2**, never a mid-loop surprise.
- **No model-named binaries.** The executable path is fixed by config; arguments may be
  model-supplied, but the binary is not. No shell interpretation by default (`execve`, not
  `/bin/sh -c`), so the model cannot inject shell metacharacters. A shell opt-in for the cases
  that genuinely need it is loudly documented as widening the surface.
- **Same OS regime.** Each `exec` child is its own process group (`setpgid`), carries a
  mandatory finite deadline, counts against the subtree token/breadth/rate budgets, and is torn
  down by the same bounded SIGTERM→SIGKILL kill ladder as any child (RFC 0003).
- **Tagged `egress` (+`sensitive` when un-jailed),** so the Rule-of-Two budget naturally refuses
  co-locating `exec` with an untrusted-input reader. Guidance: an `exec`-scoped subagent should
  be the one *least* exposed to untrusted content — pair it with a reader whose distilled
  summary it consumes, never hand it the untrusted source directly.

Enable it explicitly:

```bash
agentd \
  --instruction "build and run the test suite, report failures" \
  --intelligence unix:/run/intel.sock \
  --enable-exec
```

> **Status.** `--enable-exec` parses today and defaults off (see `crates/agentd/src/config.rs`).
> The `exec` tool body and startup capability check land in M4 (PLAN.md). The current binary
> validates config, logs, and exits with a scaffold notice for run modes.

---

## 7. Secrets handling

Secrets are config, never model/server data, and never durable agentd state.

- **Sources: env and flag only.** The intelligence credential comes from
  `AGENTD_INTELLIGENCE_TOKEN` (or `--intelligence-token`). Secrets resolve through a single
  `resolve(name)` front door. **The config file is never a secret source.** The retired
  `command` / `oauth2` resolvers are dropped.
- **The carrier is a `Secret` newtype.** Its `Debug` and `Display` both print `***`, and it has
  **no `Serialize`**, so a secret cannot accidentally enter the JSON-lines log, a spawn payload,
  an MCP `_meta` block, or a checkpoint. The logger uses a **field allowlist**: secret-bearing
  fields are simply absent from the schema, so even content-logging cannot emit them.
- **Use sites.** The intelligence credential, and config-declared headers on the intelligence
  HTTP transport via `{{secret:NAME}}` interpolation. This is an *operator-declared* header on
  the LLM endpoint — **not** a built-in `http_request` MCP tool — so the no-built-in-tools
  invariant holds. The raw secret is materialized only at the instant of writing the wire bytes
  (after CR/LF validation, §5) and is not retained longer than the request.
- **Never persisted, never in a transcript.** A secret value never appears in a tool-call
  transcript fed back to the model, in a distilled return, or on disk.

This is already visible in the foundation today: the `Config` `Debug` impl redacts the token to
`***`, and there is a test (`token_redacted_in_debug`) asserting the value never reaches a
debug string. Pass secrets via the environment:

```bash
export AGENTD_INTELLIGENCE_TOKEN="$(cat /run/secrets/intel-token)"
agentd --instruction "…" --intelligence https://api.example/v1 --model my-model
```

---

## 8. The actual v1 flag surface

Only these security-relevant knobs exist in the binary today
(`crates/agentd/src/config.rs`). Everything tagged "(roadmap)" above is the intended v1 target,
not the current CLI.

| Flag | Env | Default | Purpose |
|------|-----|---------|---------|
| `--enable-exec` | `AGENTD_ENABLE_EXEC` | off | register the gated `exec` self-tool |
| `--intelligence-token <T>` | `AGENTD_INTELLIGENCE_TOKEN` | — | bearer/key for the intelligence endpoint (redacted in logs) |
| `--intelligence <URI>` | `AGENTD_INTELLIGENCE` | — | `unix:/path`, `https://host/…`, or `vsock:cid:port` (validated; `http://` is dev-only and warns) |
| `--serve-mcp <unix:/path>` | `AGENTD_SERVE_MCP` | off | serve agentd's own MCP over a unix socket (stdio always available) |
| `--mcp name=cmd` | — | — | declare an MCP server (repeatable; stdio; operator-only, never model-derived) |
| `--max-steps <N>` | `AGENTD_MAX_STEPS` | 50 | per-run step cap (a bound on a runaway/injected loop) |
| `--max-tokens <N>` | `AGENTD_MAX_TOKENS` | 200000 | token budget |
| `--deadline <dur>` | `AGENTD_DEADLINE` | 600s | wall-clock deadline |
| `--max-depth <N>` | — | 4 | subagent tree depth cap |

The intelligence-URI validator rejects anything outside `unix:` / `https://` / `vsock:` /
`http://` and exits **2** on a bad value — before any side effect, including any LLM round-trip.

**Not yet flags** (roadmap, RFC 0012): `--allow-trifecta`, per-tool tags, the SSRF policy knobs,
and self-MCP-over-HTTP. They are documented above as the v1 target so the posture is reviewable
now.

---

## 9. Self-MCP serving: stdio / unix only in v1

Serving the self-MCP over Streamable HTTP would expose `subagent.*`, `exec`, and state
resources to network peers — a materially larger attack surface than stdio. A *safe* HTTP
server requires all of: high-entropy non-deterministic session IDs; sessions that are **not**
authentication; **no token passthrough** (MCP MUST NOT forward a bearer it was not issued);
`Origin` validation with `403` on mismatch (DNS-rebinding defense) plus loopback binding; and
the full POST+GET / SSE / protocol-version / resumability surface.

Because v1 has **no auth model** (by the §thesis decision) and none of this hardening built:

- v1 serves the self-MCP over **stdio (always)** and **unix-socket** (`--serve-mcp unix:…`,
  NDJSON framing) only.
- A unix socket inherits **filesystem permissions** as its access control — the operator sets
  the socket mode/owner. That is structural, out-of-band auth, not an in-band token.
- **Self-MCP-over-HTTP, an auth model, and `MCP-Session-Id` handling are deferred** (RFC 0013).
  agentd ships no network-exposed control surface it cannot yet secure.

---

## 10. What agentd explicitly does NOT do

Stated plainly so you size the surrounding environment correctly:

- **No policy engine / DSL, no request signing, no JWT/OAuth/x509 auth, no built-in RBAC** — in
  core, in any feature gate. The conscious reversal of "governance is the moat."
- **No in-binary sandboxing** (seccomp / namespaces / chroot) — delegated to the outer boundary
  (§1).
- **No content-based injection detection / classifier.** Prompt injection is unsolved; agentd
  defends *structurally* (isolation + scope budget + firewall) and is honest that this is
  **containment, not a guarantee**. There is no "is this prompt injection?" model call.
- **No schema-enforced subagent returns in v1** — the firewall holds on process isolation;
  constrained-shape returns are a documented defense-in-depth recommendation (§3).
- **No dynamic / network-supplied config.** Config is never read from the network; the model can
  never register an MCP server or an `exec` binary.

---

## TL;DR for operators

1. Run agentd inside a real sandbox (container/VM) with an egress NetworkPolicy and cgroup
   limits — that is the security boundary, not agentd.
2. Treat every `--mcp` server as code you execute at agentd's privilege. Vet it.
3. Keep `exec` off unless you need it; when you do, never co-locate it with an
   untrusted-content reader.
4. Tag your tools and split trifecta tasks into reader/actor subagents (roadmap M6;
   `--allow-trifecta` to override with an audit log).
5. Pass secrets via env/flag only; they never touch logs, transcripts, the config file, or disk.
