# Security Policy

`agentd` runs LLM-driven workflows that touch the filesystem, the
network, and external tools. Its security story is **architectural**, not
prompt-engineered — the design notes below describe what that buys you and
where the boundaries are. If you're deploying it, read this alongside
[`docs/maturity.md`](docs/maturity.md) (honest, dated readiness) and
[`docs/operations.md`](docs/operations.md) (hardening).

## Supported versions

| Version | Supported |
|---|---|
| `1.0.x` | ✅ Active. Security fixes land here. |
| `< 1.0` | ❌ Pre-1.0 tags are historical; upgrade to `1.0.x`. |

`agentd` follows semantic versioning from 1.0.0. Security fixes ship in
patch releases.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report privately, either way:

- Email **andrii@tsok.org** with a description, affected version /
  commit, and a reproduction. Use the subject prefix `[agentd-security]`.
- Or open a **GitHub private security advisory** on the repository
  (Security → Report a vulnerability).

What to expect:

- **Acknowledgement** within 3 business days.
- **Triage + severity assessment** within 10 business days, with a fix
  plan or a reasoned decline.
- **Coordinated disclosure**: a fix and an advisory before public
  details, target window **90 days** — sooner for actively-exploited
  issues, negotiable for complex ones. Credit given unless you'd rather
  stay anonymous.

Please report responsibly: no automated exfiltration of others' data, no
denial-of-service against shared infrastructure, no pivoting beyond what's
needed to demonstrate the issue.

## Threat model

The unit of correctness is a **frozen, validated graph**. Everything the
process can do is enumerable from the workflow TOML plus the compile-time
feature flags, *before anything runs*. That single property is what the
rest of the model hangs off.

### What the runtime defends

- **Control-flow integrity by construction.** The plan is fixed before
  any untrusted data is read ("plan-then-execute"). Tool output, webhook
  bodies, and model text can corrupt a *value* but can never add a node,
  an edge, or a capability at runtime. There is no code path from data to
  program.
- **The graph boundary is the audit boundary.** Reviews, threat models,
  and diffs operate on one declarative artifact. `agentd --validate-only`
  surfaces the whole reachable surface; `agentd inspect` replays exactly
  what a run did.
- **Compile-time capability pruning.** Capabilities are Cargo features. A
  binary built without `tools-shell` / `tools-http` cannot exec a shell
  or open an outbound socket — no runtime misconfiguration can restore a
  leg that isn't compiled in. This is the lethal-trifecta cut (private
  data + untrusted content + exfil channel) made structural.
  (`tools-http-tls` extends the same leg to HTTPS; it implies
  `tools-http`, obeys the same URL+method allowlist, and never follows
  redirects — the policy decision applies to the exact URL reached.)
- **Fail-closed least privilege.** Empty `[policy]` sections deny. The
  fs / env / http / shell / mcp allowlists gate every side effect;
  optional Rego (`policy-rego`) layers on top with AND semantics. Denials
  name the operation and the path and land in the audit stream.
- **Bounded everything.** `[budget]` caps memory (RLIMIT_AS), CPU
  (RLIMIT_CPU), wall-clock, cumulative fs-write bytes, and LLM tokens.
  `MAX_STEPS`, per-node retries, and a run deadline bound traversal.
  A run cannot consume the host.
- **Authenticated, rate-limited triggers.** HTTP triggers support bearer
  / HMAC-SHA256 / mTLS / OIDC with per-route token-bucket rate limits;
  constant-time token compare; webhook timestamp-skew checks.
- **Provenance + integrity.** Workflows are ed25519-signable
  (`signing`); the engine verifies before executing a signed workflow.
- **Autonomy is opt-in and cannot self-escalate.** Generated plans
  (instruction mode) never run headless without an explicit
  `--auto-approve` / `auto_approve = true`. A compiled plan is grafted
  onto the base environment in a way that **preserves** the base policy,
  backends, MCP set, budget, and auth — the agent cannot widen its own
  policy. Promotion (`--promote`) freezes a reviewed plan into a
  signable, self-contained workflow.

### Secrets

- Secrets are referenced by **indirection only** — `api_key_env` names an
  environment variable; the secret value never appears in the workflow
  TOML. (Post-1.0, pluggable secret providers resolve into the same
  indirection — see the roadmap.)
- `Debug` impls never print key material; the audit sink redacts a
  built-in mask list plus operator-supplied fields.
- Mount secrets from your orchestrator (k8s `Secret`, systemd
  `EnvironmentFile`, a Vault Agent sidecar, SOPS) — see below.

### Prompt-injection posture

`llm_infer` output is **data, not control**. A hostile document can
change what a value *is*, but the model cannot invent a capability, reach
a tool the policy doesn't allow, or alter the graph. The conformance
suite ships a security / injection corpus
([`docs/CONFORMANCE.md`](docs/CONFORMANCE.md)) that asserts this under a
battery of injection attempts.

### Out of scope — your responsibility

`agentd` is one bounded process. These live upstream of it by design:

- **Secret storage / rotation** — mount from your orchestrator; agentd
  consumes env / files, it does not manage a secret store.
- **Multi-tenant isolation** — run one process per tenant; agentd is
  single-tenant by shape, not a shared sandbox.
- **Network-edge DoS** — front it with a load balancer / WAF; the
  per-route rate limit is a backstop, not an edge.
- **Durability across a queue boundary** — opt-in checkpoint/resume gives
  single-node crash-recovery (at-least-once for the interrupted node);
  exactly-once across a fleet needs the queue + idempotency work on the
  roadmap.
- **The LLM provider's own security** and the integrity of model weights.

## Hardening a deployment

- Build the **narrowest binary** — only the capability features the
  workflow needs. `--all-features` is for the published image; production
  images should be purpose-built.
- Run the **distroless image** (`nonroot`, uid:gid `65532`, no shell) or
  the **systemd unit** (`DynamicUser`, `ProtectSystem=strict`, empty
  `CapabilityBoundingSet`, `MemoryDenyWriteExecute`, restrictive
  `SystemCallFilter`). See [`docs/operations.md`](docs/operations.md) §8
  and [`packaging/`](packaging/).
- **Verify release artifacts.** Tagged releases publish a `SHA256SUMS`
  file and a cosign-signed, SBOM-attested container image. Verify the
  signature and attestation before deploying (commands in
  `docs/operations.md` §8.4).
- Keep `[policy]` **fail-closed** and sign production workflows.
- Use `--dry-run` to inspect side effects before going live.

## Disclosure history

No advisories yet. Resolved issues will be listed here with their
advisory IDs.
