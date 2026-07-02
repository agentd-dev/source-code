# Security

> Spec: [RFC 0012 — Security posture](../rfcs/0012-security-posture.md). Binding
> decisions: [`docs/design/00-architecture-assessment.md`](design/00-architecture-assessment.md).
> Milestones: [`docs/design/PLAN.md`](design/PLAN.md).

agentd connects an LLM-driven loop to *arbitrary, operator-declared* MCP servers. It runs
**no local code of its own** — there is no `exec`/shell tool and no way for the model to run
a command; every capability it has is a tool from a declared MCP server. That still leaves
the one unsolved problem in agent security: **prompt injection** — and its acute form,
Willison's **lethal trifecta**, where a single agent simultaneously (1) reads untrusted
content, (2) holds sensitive data or tools, and (3) can communicate externally. Any agent
holding all three legs is a one-injected-prompt exfiltration tool.

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
6. **No local execution.** agentd runs no code of its own — no `exec`/shell tool exists; the
   only process it launches is a re-exec of the trusted agentd binary itself (a subagent),
   never a user- or model-supplied argv.
7. **Every network surface is HTTPS with authenticated identity** — intelligence, the MCP
   client, the served self-MCP, A2A, and operator control are all HTTP(S) with mTLS/bearer
   auth (loopback `http://` for dev); agentd links no unix/vsock transport.
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

agentd ships **no task tools of its own** — only its self/control orchestration tools
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
| `egress` | tool can move data out of the trust boundary or change external state — HTTP POST, send mail, open PR |

**Untagged tools default to `untrusted_input: true`** — the safe assumption is that any tool's
output may carry an injection; operators downgrade explicitly. The self/control tools
carry fixed tags: resource read/subscribe inherit from the underlying server; `subagent.*` is
untagged (it is the chokepoint, not a leaf
capability).

The tag set is a **budget, not an allow/deny rule**. It bounds what a single isolation unit
(one subagent process) may *simultaneously* hold.

> **Status.** Per-tool tagging via MCP server config (`--mcp-tags`) and the Rule-of-Two check
> below are implemented (`sec/scope.rs`). The tags JSON below and `--allow-trifecta` are on the
> CLI today; the full flag set is listed in §8.

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

### The Rule-of-Two check — one validation authority

The trifecta check lives **inside `Config::validate()`** — the single validation authority (RFC
0017 §7) that both startup and `--validate-config` run — over the **root grant**, the OR of the
capability tags across every granted MCP server (an untagged server counts conservatively as
`untrusted_input`). Because it is part of `validate()`,
`--validate-config` and startup can never disagree: a trifecta-only config that startup refuses
is also reported `config.invalid` (exit 2) by the admission gate. Because scope narrows
**monotonically** down the tree (a child's grant is always a subset of its parent's, RFC 0009),
bounding the root bounds every descendant — so the single root check suffices and the per-spawn
path never has to re-evaluate it. On a **hot reload** the check re-runs over the new config: a
reload that would newly form a complete trifecta without `--allow-trifecta` is rejected
(`config.reload_rejected{reason:"trifecta_required"}`) and the running config is kept verbatim —
the live capability set can never be widened into a trifecta without a restart.

- **Refuse** (default): a root grant that co-locates all three trifecta legs makes agentd
  **refuse to start** — `validate()` rejects it as a config error, so it prints the reason and
  exits `2` (a config-usage refusal; the daemon never comes up):

  ```text
  agentd: refused — this grant gives one agent all three lethal-trifecta legs
  (untrusted input + sensitive data + egress). Split the capabilities across
  subagents, or relaunch with --allow-trifecta.
  ```

- **Warn** (with `--allow-trifecta`): startup proceeds and the supervisor emits an auditable
  log event so the override is never silent (also emitted if a reload lands an allowed trifecta):

  ```json
  {"level":"warn","event":"scope.trifecta_grant","allowed":true,
   "legs":["untrusted_input","sensitive","egress"]}
  ```

  `--allow-trifecta` is **process-global** and does **not** propagate into spawn payloads — a
  child cannot re-grant itself the override.

- **Ok**: silent.

The check is **purely structural**. It never inspects content and never asks the model to judge
safety. The per-spawn `subagent.spawn` chokepoint does **not** re-run it: it only **narrows**
scope by intersection (a child requesting a tool its parent doesn't hold is refused as an
`isError` tool result) and clamps limits. Because a child's tag union can never exceed its
parent's, the one root-startup check bounds the whole tree.

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
- **Endpoints are never model- or server-derived.** The set of MCP servers and their endpoints
  come *only* from operator config (`--mcp`), validated at startup (bad config → exit 2). The
  model cannot add a server, edit an endpoint, or make agentd connect to a URL it produced.
  agentd never *spawns* a server — it **connects** to a declared HTTPS endpoint; the only process
  it launches is a re-exec of its own trusted binary (`subagent.spawn` re-execs `argv[0]` — §6).

> **Declaring an MCP server is an operator trust decision.** `--mcp name=https://…` points agentd
> at a remote tool endpoint you have chosen; trust it the way you trust any dependency you call.
> Its tools run in *its* sandbox, over the network — agentd runs none of its code.

- **The MCP transport is HTTPS.** A remote MCP server is reached over TLS with per-server auth
  headers (loopback `http://` for a same-host dev sidecar); agentd links no unix/vsock dialer.

---

## 5. SSRF defenses in the hand-rolled HTTP client

agentd's single hand-rolled HTTP/1.1 + SSE client is the **only** outbound network primitive,
and therefore the only SSRF chokepoint. It carries every network surface — the `https://`
intelligence transport, the MCP client, A2A, and the served self-MCP. Guards apply **after DNS
resolution and on every redirect hop**:

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

> **Status.** The SSRF guards are implemented (`net/ssrf.rs`). The HTTPS/private-range policy
> knobs described in RFC 0012 (e.g. allowing localhost/plaintext for dev) are not yet exposed
> as CLI flags.

---

## 6. No local execution

agentd **runs no code of its own.** There is no `exec` tool, no shell tool, no plugin loader —
nothing the model can call that runs a local command. Every capability the agent has is a tool
served by a declared MCP server, reached over the network (HTTPS). This removes the strongest
egress leg of the lethal trifecta *by construction*: an injected prompt cannot make agentd run
a binary, because agentd has no code path that runs one.

The one and only process agentd ever launches is a **re-exec of its own trusted binary** — a
subagent (`subagent.spawn` re-execs `argv[0]`, the agentd executable). That path never takes a
user- or model-supplied command:

- The executable is **fixed to agentd's own path**, passed by the supervisor at startup
  (`current_exe()`), never derived from a request. Freezing this is a load-bearing invariant —
  if `argv[0]` ever became request-controlled, subagent-spawn would become arbitrary-exec.
- The child's work — its instruction, tool scope, limits — arrives as a serialized payload over
  the child's **stdin pipe**, i.e. *data to a model loop*, never *code to a shell*.
- Every child is its own process group (`setpgid`), carries a finite deadline, counts against the
  subtree token/depth/breadth caps, and is torn down by the bounded SIGTERM→SIGKILL kill ladder
  (RFC 0003).

If a workflow genuinely needs to run a command (build a project, run a test suite), that belongs
behind an **MCP server** the operator declares and scopes — where it carries capability tags and
is subject to the same Rule-of-Two budget as any other tool, and where the blast radius is the
server's own sandbox, not agentd's process.

---

## 7. Secrets handling

Secrets are config, never model/server data, and never durable agentd state.

- **Sources: env and flag only.** The intelligence credential comes from
  `AGENT_INTELLIGENCE_TOKEN` (or `--intelligence-token`). Secrets resolve through a single
  `resolve(name)` front door. **The config file is never a secret source.** The retired
  `command` / `oauth2` resolvers are dropped.
- **The carrier is `Config.intelligence_token`.** The `Config` `Debug` impl maps it to `***`,
  so a secret cannot accidentally enter the JSON-lines log, a spawn payload, an MCP `_meta`
  block, or a checkpoint. The logger uses a **field allowlist**: secret-bearing fields are
  simply absent from the schema, so even content-logging cannot emit them.
- **Use site.** The intelligence credential is materialized only at the instant of writing the
  wire bytes (after CR/LF validation, §5) — set on the LLM endpoint's authorization / `x-api-key`
  header — and is not retained longer than the request.
- **Never persisted, never in a transcript.** A secret value never appears in a tool-call
  transcript fed back to the model, in a distilled return, or on disk.

This is already visible in the foundation today: the `Config` `Debug` impl redacts the token to
`***`, and there is a test (`token_redacted_in_debug`) asserting the value never reaches a
debug string. Pass secrets via the environment:

```bash
export AGENT_INTELLIGENCE_TOKEN="$(cat /run/secrets/intel-token)"
agentd --instruction "…" --intelligence https://api.example/v1 --model my-model
```

---

## 8. The actual v1 flag surface

These security-relevant knobs exist in the binary today
(`crates/agentd/src/config.rs`).

| Flag | Env | Default | Purpose |
|------|-----|---------|---------|
| `--allow-trifecta` | — | off | permit all three capability legs in one subagent (audited override) |
| `--mcp-tags name=tag,tag` | — | — | tag a server's tools `untrusted_input` / `sensitive` / `egress` for the Rule-of-Two |
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | — | bearer/key for the intelligence endpoint (redacted in logs) |
| `--intelligence <URI>` | `AGENT_INTELLIGENCE` | — | `https://host/…` (loopback `http://` for a same-host dev gateway; any other scheme is exit 2) |
| `--serve-mcp <https://host:port>` | `AGENT_SERVE_MCP` | off | serve agent's own MCP over HTTP(S) with mTLS/bearer auth (loopback `http://` for dev) |
| `--mcp name=<endpoint>` | — | — | declare a remote MCP server over Streamable HTTP (repeatable; operator-only, never model-derived) |
| `--max-steps <N>` | `AGENT_MAX_STEPS` | 50 | per-run step cap (a bound on a runaway/injected loop) |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | 200000 | token budget |
| `--deadline <dur>` | `AGENT_DEADLINE` | 600s | wall-clock deadline |
| `--max-depth <N>` | — | 4 | subagent tree depth cap |

The intelligence-URI validator rejects any scheme outside `https://` (or a **loopback**
`http://`) and exits **2** on a bad value — before any side effect, including any LLM
round-trip. The same https-only rule holds for `--mcp`, `--serve-mcp`, and `--a2a-peer`.

---

## 9. Self-MCP serving: HTTPS with authenticated identity

The self-MCP — `subagent.*`, the `agentd://` state resources, and the operator control family —
is served over **Streamable HTTP(S)**. Trust is **never derived from the transport**; it is
established per request by an authenticated identity:

- **mTLS is the primary identity.** With `--serve-cert`/`--serve-key`/`--serve-client-ca`, the
  TLS acceptor verifies the client certificate against the pinned CA; a presented, verified cert
  mints a `Management` peer.
- **A bearer token is the alternative.** `--serve-bearer <token>` accepts a request whose
  `Authorization: Bearer …` matches in **constant time**, also minting `Management`. (The token
  is redacted everywhere.)
- **No open control plane.** A non-loopback bind **must** configure mTLS and/or a bearer — an
  unauthenticated non-loopback listener is a startup error. A loopback `http://` bind with no
  auth is allowed only for local development.
- **Operator control is the A2A admin method family** (`a2a.Drain`/`LameDuck`/`Pause`/`Resume`/
  `Cancel`), reachable only by a `Management` peer; an unauthenticated/in-process (`Stdio`) caller
  gets `-32601`, as if the method did not exist.

This is the authenticated, hardened HTTP control surface earlier drafts deferred — it now ships
(the HTTP/1.1 + SSE server framing lives in the reusable `mcp` crate). agentd links **no**
unix/vsock listener.

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
4. Tag your tools and split trifecta tasks into reader/actor subagents
   (`--allow-trifecta` to override with an audit log).
5. Pass secrets via env/flag only; they never touch logs, transcripts, the config file, or disk.
