# agentd — Documentation Index

`agentd` (codebase path: `crates/agentd/`) is a bounded,
workflow-driven runtime. Workflows are authored in TOML as a DAG of
typed nodes. The runtime validates them at build- and load-time, runs
them in one-shot or serve mode, and exposes an HTTP surface with
bearer / HMAC / mTLS authentication, in-process TLS, per-route rate
limiting, and graceful-shutdown semantics.

This directory is the authoritative documentation set. The code is the
final source of truth; these docs describe the code as it stands.

---

## Read this first

- **[quickstart.md](quickstart.md)** — five minutes from install to a
  running, inspectable agent. No API key for the first run. Start here if
  you've never run agentd.

- **[use-cases/](use-cases/README.md)** — fourteen business-automation
  patterns (voice, CRM, support, finance, incidents, hiring, …), each a
  general-audience article + a validated sample workflow, with an honest
  [capability gap analysis](use-cases/GAP-ANALYSIS.md). Start here if
  you're deciding *whether* to use agentd.

- **[architecture.md](architecture.md)** — mental model, module layout,
  execution lifecycle, data flow, invariants. Start here if you want to
  modify the runtime.

- **[capabilities.md](capabilities.md)** — complete node catalogue
  (all 22 `NodeKind` variants), edge rules, start nodes, triggers,
  HTTP routes, policy grammar, auth, TLS, rate limiting, retries,
  input resolution, execution outcome + exit codes, test harness.
  Start here if you're authoring a workflow.

- **[configuration.md](configuration.md)** — every TOML field, every
  CLI flag, every `AGENTD_*` env var, every Cargo feature, with
  precedence rules and a canonical hardened-webhook example.

- **[operations.md](operations.md)** — build modes, deployment shapes
  (one-shot / serve / embedded), TLS + cert management, logging
  targets, shutdown semantics, exit codes, runbook basics, k8s +
  systemd templates.

- **[maturity.md](maturity.md)** — honest production-readiness
  snapshot. Green / yellow / red status by concern, named gaps with
  effort sizing, target deployment bars, test-coverage snapshot.

## Design record

- **[RFC 0001 — Harness Workflow Runtime](../../rfcs/0001-bounded-workflow-runtime.md)**
  — the original design. The "Implementation Status" section at the
  top maps every RFC section to what actually shipped through R5 + R3b
  and the standalone-only pivot.

## Archive

- **[archive/](archive/)** — documents that were accurate at a point
  in time but have since been superseded by a design pivot or
  refactor. Kept for traceability; `archive/README.md` has the index.

---

## Quick pointers

| If you want to… | Start here |
|---|---|
| Run agentd for the first time | `quickstart.md`. |
| Author your first workflow | `capabilities.md` → §1 mental model, then the §"Node catalogue". |
| Understand the TOML grammar | `configuration.md`. |
| Deploy the binary | `operations.md`. |
| Know what's safe to rely on in prod | `maturity.md`. |
| Know what the compile-time feature flags do | `configuration.md` §Build modes / `operations.md` §2. |
| Debug a workflow failure | Outcome JSON contains a per-node execution trace — see `capabilities.md` §Execution outcome. |
| Modify the runtime itself | `architecture.md` first, then the relevant source module. |
| See the shipping test surface | `maturity.md` §4 + `crates/agentd/tests/`. |
| See the original design intent | RFC 0001. |

---

## Short status

- **Single-entry-point binary.** No CLI subcommands. Mode is inferred
  from the workflow (`[[http_routes]]` present → serve; absent →
  one-shot); override with `--mode`.
- **Statelessness.** The runtime holds no persistent state. Restart is
  free; crash loses only in-flight requests.
- **Security defaults.** `[policy]` is fail-closed; default feature
  set ships `auth`. `server-tls` is opt-in.
- **Test suite:** 471 tests across the crate + 4 integration test
  binaries; stable under the feature matrix documented in
  `configuration.md`.

Questions or gaps? File an issue, reference the doc page.
