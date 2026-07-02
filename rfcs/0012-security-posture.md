# RFC 0012: Security posture

> **⚠ AMENDED (target-vision pivot, 2026-07-02).** The `exec` self-tool described
> here was **removed** — agentd runs no local code (no exec/shell tool). Every
> transport is HTTPS (no unix/vsock). See [`../docs/design/00-target-vision-pivot.md`](../docs/design/00-target-vision-pivot.md).

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

agentd connects an LLM-driven loop to *arbitrary, operator-declared MCP servers* and an
`exec` capability. That is, by construction, the worst-case shape for the single unsolved
agent-security problem: **prompt injection / the lethal trifecta** (Willison) — an agent
that simultaneously (1) reads untrusted content, (2) holds sensitive data/tools, and
(3) can communicate externally is a one-injected-prompt exfiltration tool. MCP makes this
*worse by design*: it encourages mixing tools from many sources, and most clients accept
server-supplied tool descriptions/schemas without validation (tool poisoning, OWASP ASI01).
Prompt injection is not patchable; 95%-effective guardrails are failures in security terms.

The retired design answered this with "governance is the moat" — a policy DSL (regorus),
signing (ed25519), JWT auth, x509. **This RFC is the conscious reversal of that
(assessment §2.11).** A policy engine inside an injectable model loop is theatre: the model
can be steered to produce policy-compliant-but-malicious actions, and the engine is pure
binary weight and attack surface. agentd's security is **minimalism + structural isolation**:
the OS process tree, the granted-MCP-subset interpreted as a trust budget, and distilled
structured returns as an injection firewall. That is honest about what is and is not solvable,
and it costs near-zero binary weight — consistent with the minimalism bar that is the moat.

This RFC specifies the security mechanisms that *are* in-core (scope tagging + Rule-of-Two
checks, untrusted-content stance, SSRF guards in the hand-rolled HTTP client, gated `exec`,
self-MCP transport hardening, secrets handling) and is explicit about what is delegated to
the deployment boundary (sandboxing, network policy, cgroups, TLS termination).

## 2. Decision

**No policy engine, no signing, no auth as core.** Security is structural:

1. **The outer boundary is the sandbox.** Container / VM / microVM / enclave provides
   confinement, egress policy, filesystem scope, and resource limits. agentd does **not**
   reimplement any of these. It is sandbox-*aware* (cgroup-v2, §2.8 of the assessment) but
   never sandbox-*providing*.
2. **Capability scoping = the granted MCP subset, read as a Rule-of-Two trust budget.** Scope
   narrows monotonically down the subagent tree (RFC 0009). Tools carry operator-declared tags
   (`untrusted_input` / `sensitive` / `egress`); the spawn chokepoint **warns or refuses** a
   grant that hands one subagent all three trifecta legs without `--allow-trifecta`.
3. **Process isolation + distilled structured returns = CaMeL-style separation / injection
   firewall.** An untrusted-content reader subagent holds *no* sensitive/egress tools and
   returns a constrained, distilled summary (RFC 0009 §result) to a parent that holds sensitive
   tools but **never sees the raw untrusted content**.
4. **All MCP server content is untrusted — including tool descriptions, schemas, and
   annotations** (tool poisoning / ASI01). Never build a launch command from a
   model- or server-controlled string. stdio default limits a server's reach to agentd.
5. **SSRF defenses live in the hand-rolled HTTP client** (RFC 0006 / `net/http.rs`):
   HTTPS-in-prod, block RFC-1918 / loopback / link-local by default, validate redirects, pin
   DNS where feasible, CR/LF-rejecting header construction, localhost opt-out for dev.
6. **`exec` is off by default**, capability-checked at startup, folded into the same OS
   limits + kill ladder (RFC 0003), and is the leg that should be *least* exposed to untrusted
   content.
7. **Self-MCP serving prefers stdio / unix-socket; HTTP serving is deferred** (RFC 0013)
   precisely because Streamable HTTP serving needs real hardening agentd does not yet do.
8. **Secrets are env/flag only**, behind a `resolve()` front door, never logged / persisted /
   put in a transcript; their `Debug` prints `***`.

These live primarily in `sec/{secrets,scope,exec}.rs` and `net/http.rs`. The Rule-of-Two
tag check and SSRF guards land in **M6** (assessment §4, M6); secrets and the scope grant
land in **M1/M2**; gated `exec` in **M4**.

---

## 3. Mechanisms

### 3.1 Tool tags and the trust-budget model

A *tool tag* is an operator-declared property of a tool, not a model- or server-declared one
(server metadata is untrusted, §3.4). Tags are set in MCP server config alongside the launch
command:

```rust
bitflags-free, just an enum-set:

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ToolTags {
    pub untrusted_input: bool, // tool returns content from an uncontrolled source
                               //   (web pages, inbound email, issue text, arbitrary files)
    pub sensitive:       bool, // tool exposes private data or privileged systems
                               //   (secrets store, internal DB, prod control plane)
    pub egress:          bool, // tool can move data out of the trust boundary or change
                               //   external state (HTTP POST, send mail, open PR, exec)
}

impl ToolTags {
    pub fn trifecta(self) -> bool { self.untrusted_input && self.sensitive && self.egress }
    pub fn legs(self) -> u8 {
        self.untrusted_input as u8 + self.sensitive as u8 + self.egress as u8
    }
}
```

Config wire (per the MCP server config of RFC 0004, `--mcp-config FILE` / `--mcp` flags):

```json
{
  "mcp": {
    "web":   { "cmd": ["mcp-fetch"],   "tags": { "*": ["untrusted_input"] } },
    "vault": { "cmd": ["mcp-vault"],   "tags": { "*": ["sensitive"] } },
    "mail":  { "cmd": ["mcp-smtp"],    "tags": { "send_*": ["egress"] } }
  }
}
```

Tags attach by tool-name glob (first-match, longest-glob-wins) so a server can be split
(e.g. a `read_*` subset that is `sensitive` but not `egress`). **Untagged tools default to
`untrusted_input: true`** — the safe default is to assume a tool's output may carry injection;
operators downgrade explicitly. The built-in self-MCP tools (RFC 0005) carry fixed tags:
`exec` ⇒ `egress` (and `sensitive` when not jailed), `resource.read`/`subscribe` ⇒ inherit
from the underlying server, `subagent.*` ⇒ untagged (the chokepoint, not a leaf capability).

The tag set is a *budget*, not an allow/deny rule: it bounds what one isolation unit
(a single subagent process) may simultaneously hold.

### 3.2 The Rule-of-Two check at the spawn chokepoint

Scope grants flow only through the supervisor-owned `subagent.spawn` (RFC 0009 — the single
unforgeable chokepoint; depth minted by the supervisor). When a parent requests a child scope
`grant ⊆ parent_scope`, the supervisor computes the union of tags across the granted tools and
evaluates the budget *before* re-exec:

```rust
pub enum TrifectaVerdict { Ok, Warn(ToolTags), Refuse(ToolTags) }

pub fn check_trifecta(grant: &Scope, allow_trifecta: bool) -> TrifectaVerdict {
    let mut u = ToolTags::default();
    for t in grant.tools() { u.union_in_place(t.tags); }   // OR across the granted set
    if u.trifecta() {
        if allow_trifecta { TrifectaVerdict::Warn(u) } else { TrifectaVerdict::Refuse(u) }
    } else {
        TrifectaVerdict::Ok
    }
}
```

- **`Refuse`** (default): `subagent.spawn` returns an MCP **tool result** with `isError:true`
  (never a crash, never a JSON-RPC error — per RFC 0007 the parent's model sees it as an
  observation and adapts), e.g.

  ```json
  {"isError": true,
   "content":[{"type":"text",
     "text":"refused: this grant gives one subagent all three lethal-trifecta legs
             (untrusted_input + sensitive + egress). Split into reader/actor subagents,
             or relaunch agentd with --allow-trifecta to override."}]}
  ```

- **`Warn`**: with `--allow-trifecta`, the spawn proceeds and the supervisor emits a
  `limit.exceeded`-class log event (`event:"scope.trifecta_grant"`, `level:"warn"`, fields:
  `agent_path`, child `agent_id`, `legs`, the offending tool names) so the override is
  auditable. The flag is process-global and **does not** propagate into spawn payloads — a
  child cannot re-grant itself the override; it remains the supervisor's decision per spawn.

- **`Ok`**: silent.

The check is purely structural — it never inspects content or asks the model to judge. It is a
*budget on co-located capability*, so the recommended pattern is encoded in the
`subagent.spawn` tool description (RFC 0005, RFC 0009): split a trifecta task into a
no-sensitive/no-egress **reader** that returns a distilled summary and a no-untrusted-input
**actor** that consumes the summary.

Because scope narrows monotonically down the tree (RFC 0009), a child can never widen its tag
union beyond the parent's; the budget is enforced at every level by the same chokepoint.

### 3.3 Distilled returns as an injection firewall

This is the load-bearing structural defense and it falls out of the subagent result contract
(RFC 0009): a child returns a **distilled, structured value (~1–2k tokens) + terminal status +
usage** up the length-framed control channel (RFC 0005); the parent appends the *distillate*,
never the child's raw transcript. Two security properties follow with no extra mechanism:

1. **Content quarantine.** Raw untrusted bytes (a poisoned web page, a malicious tool
   description echoed in a tool result) live only inside the reader subagent's context. They
   are deleted when that process exits. The parent's context — which holds sensitive/egress
   tools — never ingests them, so an injection in that content cannot author actions in the
   parent. This is CaMeL's trusted-planner / untrusted-data split realized as OS process
   isolation rather than a taint-tracking interpreter.

2. **Bandwidth limiting.** A 1–2k-token distillate is a low-bandwidth channel; exfiltrating a
   secret *through* the summary requires the reader (which has *no* sensitive tools, so holds
   no secret to leak) to encode it — structurally impossible when scopes are split per §3.2.

Hardening of the firewall (recommended, encoded in tool descriptions, not enforced in core):
the parent specifies the child's **output contract** (RFC 0009) as a constrained shape
(enum/struct fields, not free prose) so injected instructions in the child's input have no
syntactic place to surface in the return. agentd does not *enforce* schema-constrained returns
in v1 (it would need provider strict-mode plumbing); the firewall holds on isolation alone, and
the constrained shape is a defense-in-depth recommendation.

### 3.4 All MCP server content is untrusted

Every byte that originates from an MCP server is untrusted model input, **including the parts
the protocol presents as trusted metadata**: tool `name`/`description`/`inputSchema`,
`annotations`, resource `description`/`mimeType`, prompt text, and of course tool results
(`content[]`). Concrete rules in the MCP client (RFC 0004) and loop (RFC 0007):

- **No auto-trust of server metadata.** Tool descriptions and annotations are passed to the
  model as the tool catalogue but are never used to make a *security* decision (tags come from
  operator config, §3.1, never from `annotations`). The `readOnlyHint`/`destructiveHint`
  annotations are treated as untrusted hints, surfaced for audit, never load-bearing.
- **Audit surface.** On `tools/list`, log each tool's `{server, name, description_hash,
  description_len}` at `info` (`event:"mcp.tool.listed"`); `--log-content` additionally logs
  the full description (redaction-aware) so an operator can review for poisoning. A description
  whose hash changes between connections logs `event:"mcp.tool.description_changed"` at `warn`
  (rug-pull / TOCTOU detection).
- **Launch commands are never model- or server-derived.** The set of MCP servers and their
  `argv` come *only* from operator config (`--mcp`, `--mcp-config`), validated at startup
  (RFC 0011, exit 2 on bad config). The model cannot add a server, edit an `argv`, or cause
  agentd to spawn a process from a string it produced. `subagent.spawn` re-execs **agentd's own
  `argv[0]`** (RFC 0009), never a model-supplied path; `exec` (§3.6) runs an operator-allowed
  binary, never a server-named one. **Spawning a stdio MCP server = trusting that command as
  code at agentd's privilege** — documented as an operator trust decision equivalent to running
  the binary.
- **stdio is the default transport** (RFC 0004): a stdio server can reach only agentd's pipes,
  not the network or other processes, which is itself a confinement win over an HTTP server.

### 3.5 SSRF defenses in the hand-rolled HTTP client

The single hand-rolled HTTP/1.1 + SSE client (`net/http.rs`, RFC 0006) is the only outbound
network primitive and therefore the only SSRF chokepoint. It is used for the `https://`
intelligence transport and any future HTTP-MCP. Guards, applied **after DNS resolution and on
every redirect hop**:

```rust
pub struct HttpPolicy {
    pub require_https: bool,        // default true; AGENT_HTTP_ALLOW_PLAINTEXT=1 to relax
    pub allow_private: bool,        // default false (block RFC-1918/loopback/link-local/ULA)
    pub allow_localhost: bool,      // dev opt-out: --http-allow-localhost
    pub max_redirects: u8,          // default 5
    pub pin_resolved_ip: bool,      // connect to the IP we vetted, not a re-resolve
}

fn vet_addr(ip: IpAddr, pol: &HttpPolicy) -> Result<(), Ssrf> {
    let blocked = match ip {
        IpAddr::V4(v4) =>
            v4.is_private()            // 10/8, 172.16/12, 192.168/16
            || v4.is_loopback()        // 127/8
            || v4.is_link_local()      // 169.254/16  (incl. 169.254.169.254 cloud metadata)
            || v4.octets()[0] == 0     // 0.0.0.0/8
            || v4.is_broadcast() || v4.is_documentation(),
        IpAddr::V6(v6) =>
            v6.is_loopback()           // ::1
            || v6.is_unique_local()    // fc00::/7
            || v6.is_unicast_link_local() // fe80::/10
            || is_v4_mapped_private(v6),// ::ffff:10.0.0.0 etc — unwrap & re-vet
    };
    if blocked && !(ip.is_loopback() && pol.allow_localhost) && !pol.allow_private {
        return Err(Ssrf::BlockedRange(ip));
    }
    Ok(())
}
```

- **HTTPS in prod.** `require_https` rejects `http://` targets by default; loopback dev with
  `--http-allow-localhost` may use plaintext.
- **Block private/loopback/link-local by default.** The `169.254/16` block specifically denies
  the `169.254.169.254` cloud-metadata SSRF; the v4-mapped-v6 and `0.0.0.0/8` cases close the
  usual bypasses.
- **DNS pinning / anti-rebinding.** Resolve once, `vet_addr` the resolved IP(s), then connect
  to the *vetted* IP (`pin_resolved_ip`), not a fresh resolution — closes the DNS-rebinding
  TOCTOU between the policy check and the connect. (No `url`/IDNA crate; a hand-rolled
  authority parser per the assessment's `url`-rejection.)
- **Validate redirects.** Each `3xx` `Location` is parsed, re-vetted (scheme + `vet_addr` on
  its resolved IP), and counted against `max_redirects`; a cross-host or downgrade
  (`https`→`http`) redirect is **refused, not followed** — surfaced as the request error, never
  auto-chased. (The retired WebFetch instinct of returning the redirect to the caller is the
  right shape.)
- **CR/LF-injection-rejecting headers.** Salvage the retired header-construction guard: header
  names/values containing `\r`, `\n`, or NUL are rejected at construction (`Err(BadHeader)`),
  so a value derived from any string (including a model-produced one routed into a future
  declared-header path, RFC 0006) cannot inject a header or split the request. Header *values*
  that interpolate `{{secret:NAME}}` are resolved (§3.7) *after* this validation on the literal
  template, and the resolved secret is itself CR/LF-checked.

These are a few tens of lines of checks, not a library — consistent with the no-`url`/no-ICU
dependency stance.

### 3.6 `exec` — off by default, gated, least-exposed

`exec` (`sec/exec.rs`, RFC 0005 self-tool, RFC 0009 process regime) is the strongest egress
leg and therefore the most dangerous trifecta member. Rules:

- **Off by default.** Absent any `--enable-exec`, the `exec` self-tool is **not registered** in
  the self-MCP `tools/list` — the model never sees it. (Not "present but erroring": absent, so it
  cannot be discovered or poisoned into existence.)
- **Operator allowlist of binaries.** `--enable-exec <abs-path>` (repeatable; or
  `AGENT_ENABLE_EXEC` as a `:`-separated path list) supplies the set of absolute binary paths
  the tool may invoke. A bare `--enable-exec` with no path is a usage error (exit 2: "requires an
  allowed binary path"). The allowlist is the operator's, never the model's.
- **Capability-checked at startup.** Each allowed path is validated to exist and be executable;
  a missing or non-executable allowed binary is a **config error → exit 2** (RFC 0011), not a
  runtime surprise mid-loop. This check lives in `Config::validate()` — the one validation
  authority — so `--validate-config` and startup agree (RFC 0017 §7).
- **No model-named binaries.** A tool call whose resolved `argv[0]` is not an **exact-path
  match** against the allowlist is rejected as a tool-domain `isError` observation (the model
  adapts — never a crash/exit); arguments may be model-supplied but the executable is fixed by
  config (defense against §3.4 launch injection). No shell interpretation by default (`execve`,
  not `/bin/sh -c`), so the model cannot inject shell metacharacters; a `--exec-shell` opt-in for
  the cases that need it is loudly documented as widening the surface.
- **Same OS regime.** Each `exec` child is its own process group (`setpgid`), carries a
  **mandatory finite deadline**, is counted against the subtree token/breadth/rate budgets, and
  is torn down by the same bounded depth-first SIGTERM→SIGKILL kill ladder (RFC 0003). It has
  no control channel, so only Detector A (deadline) and the kill ladder apply — not ping/pong.
  Reference: the retired `tools/shell.rs::run()` (reader-threads + `try_wait` + timeout-kill +
  signal-extract).
- **Tagged `egress` (+`sensitive` when un-jailed).** So §3.2's budget check naturally refuses
  co-locating `exec` with an untrusted-input reader. **Guidance (encoded in the tool
  description):** an `exec`-scoped subagent should be the one *least* exposed to untrusted
  content; pair it with a reader subagent whose distilled summary it consumes, never give it the
  untrusted source directly.

### 3.7 Secrets handling

Secrets are config, never model/server data, and never durable agentd state.

- **Sources: env and file only** via the `secrets::resolve(name)` front door
  (`sec/secrets.rs`); `command`/`oauth2` resolvers from the retired design are **dropped**. The
  config-file path (RFC 0011) is **never** a secret source — secrets are env/flag only
  (assessment §2.10). **The exact resolution order and the `resolve(name)` signature are owned by
  RFC 0006 §6** (the credential surface, salvaged from the retired `secrets/mod.rs`): a configured
  source for `name` (env-alias or a live-read `file`), else the process environment variable
  `name`. This RFC owns only the *policy* the carrier must satisfy (never logged / persisted /
  in-transcript; `Debug` = `***`); it does not redefine the lookup.

  ```rust
  pub struct Secret(String);                 // newtype; the only carrier (RFC 0006 calls it Token)

  impl std::fmt::Debug   for Secret { fn fmt(&self,f:&mut Formatter)->Result { f.write_str("***") } }
  impl std::fmt::Display for Secret { fn fmt(&self,f:&mut Formatter)->Result { f.write_str("***") } }
  // no Serialize; Secret cannot be accidentally serialized into a transcript/log/checkpoint.

  // resolve(name) — owned by RFC 0006 §6; resolution order defined there.
  ```

- **`Debug`/`Display` print `***`.** The newtype has no `Serialize`, so it cannot enter the
  JSON-lines logger (RFC 0010), a spawn payload (RFC 0009), an MCP `_meta` block, or a v2
  checkpoint (RFC 0013). Logging uses a **field allowlist** (assessment §2.9): secret-bearing
  fields are simply absent from the schema, so even `--log-content` cannot emit them.
- **Use sites:** the intelligence credential (`AGENT_INTELLIGENCE_TOKEN` + provider-specific,
  RFC 0006, build-time key probe → fast-fail) and **config-declared** headers on the
  intelligence HTTP transport via the `{{secret:NAME}}` interpolation (RFC 0006 §3 — the
  salvaged `substitute_secret_placeholders`/`render_declared_headers`; this is an
  operator-declared header on the LLM endpoint, **not** a built-in `http_request` MCP tool —
  the no-built-in-tools invariant of RFC 0001 §2 holds). The
  raw secret is materialized only at the moment of writing the wire bytes, after CR/LF
  validation (§3.5), and is never retained on the heap longer than the request.
- **Never persisted, never in a transcript.** A secret value never appears in a tool-call
  transcript fed back to the model, in a distilled return, or on disk. The build-time probe
  checks *presence/format*, never logs the value.

### 3.8 Self-MCP-over-HTTP hardening — why v1 defers it

Serving the self-MCP (RFC 0005) over Streamable HTTP would expose agentd's `subagent.*`,
`exec`, and state resources to network peers — a materially larger attack surface than stdio.
A conformant, *safe* Streamable HTTP server requires all of:

- **Non-deterministic session IDs** (high-entropy, e.g. 128-bit CSPRNG) in `MCP-Session-Id`;
- **Sessions are not authentication.** A session id identifies a connection, never authorizes a
  caller; an unknown id ⇒ 404 → client restarts, never an implicit grant.
- **No token passthrough** (MCP MUST NOT): agentd must not accept or forward a bearer token that
  was not issued to it; the self-MCP performs no OAuth proxying.
- **`Origin` validated → HTTP 403** on mismatch (DNS-rebinding defense), **bind to loopback**
  when local.
- POST+GET endpoint, SSE upgrade, `MCP-Protocol-Version` header, resumability — none of which
  agentd implements in v1.

Because v1 has **no auth model** (by §2 decision — no auth as core) and none of this hardening
built, **v1 serves the self-MCP over stdio (always) and unix-socket (`--serve-mcp unix:…`, NDJSON
framing) only** (RFC 0005). A unix socket inherits filesystem permissions as its access control
(operator sets the socket mode/owner) — structural, not in-band, auth. Streamable HTTP serving,
together with the hardening above and an auth story, is **deferred to RFC 0013**. This is honest
minimalism: agentd does not ship a network-exposed control surface it cannot yet secure.

### 3.9 What is delegated to the deployment boundary

Stated explicitly so operators size their environment correctly (the assessment is binding that
these are *not* in-binary):

- **Sandboxing / confinement** — container/VM/microVM/enclave. agentd does not seccomp, chroot,
  or namespace itself.
- **Egress network policy** — beyond the SSRF guards (§3.5), coarse egress control (which hosts
  the whole pod may reach) is a NetworkPolicy / firewall concern. The recommended container
  shape terminates TLS at a sidecar (assessment §2.2), so most builds link no TLS at all.
- **Aggregate memory limits** — cgroups v2 (`memory.max`/`pids.max`/`cgroup.kill`), per the
  honest caveat in assessment §2.8: only the *token* ceiling and per-child `RLIMIT_AS`/`CPU` are
  in-binary; aggregate subtree memory is a deployment concern. agentd is cgroup-aware, never
  cgroup-requiring.
- **Authn/z of inbound callers** — filesystem perms on the unix socket (v1); a real auth model
  is deferred with HTTP serving (RFC 0013).

---

## 4. Interactions with other RFCs

- **RFC 0001 (core):** ratifies "outer boundary + granted MCP subset is the security model";
  this RFC supplies the trust-budget interpretation and SSRF/exec/secrets detail.
- **RFC 0003 (supervision):** `exec` and refused/over-budget spawns ride the bounded kill ladder
  and restart governor; PDEATHSIG/subreaper ensure a crashed supervisor cannot leak an
  injected/runaway child (the worst leak — assessment §5 risk 3).
- **RFC 0004 (MCP client):** capability-gated; the untrusted-metadata stance (§3.4), tool-listing
  audit log, and never-build-launch-commands rule live here. Tags attach to its server config.
- **RFC 0005 (self-MCP + control protocol):** registers `exec` only under `--enable-exec`;
  serves over stdio/unix only; emits `tools/list_changed` on scope narrowing. Distilled returns
  (§3.3) travel its length-framed control channel.
- **RFC 0006 (intelligence transport):** owns `net/http.rs` where the SSRF guards (§3.5) and
  CR/LF header validation live; owns the `Secret` newtype use sites and `{{secret:NAME}}`
  interpolation (§3.7).
- **RFC 0007 (agentic loop):** a refused trifecta grant / over-budget spawn / `exec` failure is
  an `isError:true` **observation** the model adapts to, never a crash. VERIFY is
  environment-grounded, never self-judgment — the same anti-self-grading stance as §3.2/§3.3.
- **RFC 0009 (subagent model):** the spawn chokepoint hosts the Rule-of-Two check (§3.2);
  monotonic scope narrowing + depth minting make the budget enforceable at every level; the
  distilled-result contract is the firewall (§3.3); `exec` folds into the same caps.
- **RFC 0010 (observability):** the field allowlist keeps secrets out of logs; `scope.trifecta_grant`,
  `mcp.tool.listed`, `mcp.tool.description_changed`, and SSRF-refusal events are part of the
  closed event vocabulary; content capture (incl. tool descriptions) is `--log-content` opt-in.
- **RFC 0011 (cloud-native contract):** bad security config (missing `exec` binary, secret not
  resolvable, `--allow-trifecta` not set for a trifecta-only deployment) → validate-at-startup →
  **exit 2**, never a mid-loop surprise; secrets never come from the config file or network.
- **RFC 0013 (deferred v2):** Streamable HTTP serving + the self-MCP hardening of §3.8 + an auth
  model are deferred there.

## 5. Non-goals / Deferred

- **No policy engine / DSL** (no regorus or equivalent), **no request signing** (no ed25519),
  **no JWT/OAuth/x509 auth**, **no built-in RBAC** — the conscious reversal of "governance is the
  moat" (assessment §2.11). None of these are in core, in any feature gate.
- **No in-binary sandboxing** (seccomp/namespaces/chroot) — delegated to the outer boundary
  (§3.9).
- **No content-based injection detection / classifier.** Prompt injection is unsolved; agentd
  defends *structurally* (isolation + scope budget + firewall), and is honest that this is
  containment, not a guarantee (assessment §5 risk 7). No "is this prompt injection?" model call.
- **No schema-enforced subagent returns in v1** (§3.3) — the firewall holds on process isolation;
  constrained-shape returns are a documented defense-in-depth recommendation, not enforced.
- **Self-MCP-over-HTTP, an auth model, and `MCP-Session-Id` handling — deferred to RFC 0013**
  (§3.8). v1 has no network-exposed control surface.
- **MCP `roots` as a filesystem-scope signal** — acknowledged as idiomatic but deferred with the
  rest of the v1 client-capability deferrals (assessment §2.5); v1 declares no client capabilities.
- **cgroup write enforcement** — agentd never *requires* cgroup write access; aggregate-memory
  enforcement is the deployment's job (§3.9, assessment §2.8 caveat).
- **DNS-over-the-network config / dynamic server registration** — config is never read from the
  network (RFC 0011); the model can never register an MCP server or an `exec` binary.

## 6. Open items

- **Tag taxonomy granularity.** Three legs (`untrusted_input`/`sensitive`/`egress`) match the
  Rule-of-Two literature, but real tools are mixed (a `read_*` subset of one server may be
  `sensitive`-not-`untrusted`). The per-tool-glob tagging (§3.1) is the mitigation; whether
  operators will tag at sufficient granularity in practice, versus defaulting whole servers, is
  an empirical question to revisit after M6.
- **`--allow-trifecta` blast radius.** Today it is process-global. A per-route / per-spawn
  override (so only one specific trifecta-needing path is permitted, not the whole daemon) is a
  plausible refinement deferred until a real deployment needs it; the audit log (§3.2) is the
  interim control.
