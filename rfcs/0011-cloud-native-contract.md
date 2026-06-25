# RFC 0011: Cloud-native contract — config, signals, exit codes, idempotency

**Status:** Draft
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

`agentd` is a single binary that an external scheduler (K8s `Job`/`CronJob`/`Deployment`, Knative, a Nomad task, a bare-metal supervisor) *starts, stops, replicates, and watches*. The orchestrator itself is out of scope (assessment §2.11: "composition is MCP, not a control plane we own"). What is in scope — and what this RFC nails down — is the **contract `agentd` honours so an orchestrator can drive it correctly**: how config is sourced and validated, how it behaves on signals, what its exit codes *mean* to a `podFailurePolicy`, and how a retried run avoids duplicating side effects.

This RFC covers assessment §2.10 in implementation depth. The central tension it resolves (per `notes-review-cloud-native-unit.md` §0) is the apparent opposition between two identities:

- **Unit of work** — ephemeral, run once, emit a result, exit (`Job`/`CronJob`).
- **Reactive daemon** — long-lived, idles cheaply, wakes on MCP resource updates (`Deployment`).

These are **not two binaries and not two code paths.** They are the same supervisor loop under two *termination policies*, differing only by exit predicate (assessment §2.6, RFC 0008). The lifecycle machinery, config parsing, signal handling, drain choreography, and process-tree supervision are byte-for-byte identical. The load-bearing simplification this RFC must preserve: **resist any feature that forks the daemon and the job into divergent code.**

This RFC owns: the config-precedence rule and validate-at-startup discipline; the signal contract and drain choreography; the public exit-code table; and the `RUN_ID` idempotency mechanism. It does **not** own the kill ladder internals (RFC 0003), the health surface (RFC 0010), reactive routing (RFC 0008), or cgroup awareness (RFC 0003 — referenced here only where it touches teardown semantics).

---

## 2. Decision

1. **Config precedence is a hard rule, top wins:** `built-in default < config file < env var < CLI flag`. Everything is env-settable (12-factor III). The file is only for verbose structural lists (MCP servers), **never** for per-environment values, **never** for secrets. Validate fully at startup **before any side effect** → bad config exits `2` in milliseconds, not after an LLM round-trip. **Never read config from the network.**

2. **Signals:** `SIGTERM`/`SIGINT` → flip a one-way `DRAINING` flag → bounded drain (disarm triggers → wind down subagents at turn boundaries → ladder stragglers → flush logs → exit). A **second** `SIGTERM`/`SIGINT` → `force` → immediate `SIGKILL` of all groups. `SIGCHLD` → reap loop. `SIGPIPE` → ignored. `SIGHUP`/reload is **dropped** (restart-to-reload in v1). `AGENTD_DRAIN_TIMEOUT` **MUST be < pod `terminationGracePeriodSeconds`** (default 25 s vs 30 s; validated and warned at startup).

3. **The exit-code table (§5) is a public, machine-actionable API** for `podFailurePolicy`. A clean SIGTERM drain returns **0, not 143.** One-shot maps the root subagent's terminal status to a code. loop/reactive daemons exit only `0`, `143`, or a fatal class.

4. **Idempotency:** accept `AGENTD_RUN_ID`/`--run-id` (default a per-process ULID); propagate it into every MCP tool-call `_meta` so backing services dedupe retries. Encourage read-modify-write-through-MCP and make "already done" cheap → exit `0`. `agentd` introduces **no local non-idempotent side effects** — it has no built-in tools, so all durable output is externalized through MCP, and this property falls out structurally.

These decisions are final for v1. Each defers to the assessment doc where it defers (noted inline).

---

## 3. Mechanisms — config

### 3.1 Precedence resolution

Config is assembled in four layers, each overriding the previous key-by-key:

```rust
// config.rs
pub struct Config {
    pub mode: Mode,                       // once | loop | reactive | schedule (RFC 0008)
    pub instruction: Option<String>,      // required for once/loop; None legal for reactive
    pub intelligence: IntelUri,           // unix: | https: | vsock:
    pub intelligence_token: Option<Secret>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub mcp_servers: Vec<McpServerSpec>,
    pub interval: Option<Duration>,       // loop mode only
    pub subscribe: Vec<String>,           // reactive mode
    pub serve_mcp: Option<ServeAddr>,
    pub enable_exec: ExecPolicy,
    pub limits: Limits,                   // max_steps/tokens/deadline/depth/tree_token_budget
    pub drain_timeout: Duration,          // default 25s
    pub log_format: LogFormat,            // json (default in container) | text
    pub log_level: Level,
    pub health_file: Option<PathBuf>,
    pub health_addr: Option<SocketAddr>,  // None => no listener (off for one-shot)
    pub run_id: Ulid,                     // §6
    pub cgroup: Option<PathBuf>,          // auto-detect default
}

fn load() -> Result<Config, ConfigError> {
    let mut c = Config::builtin_defaults();   // layer 0
    if let Some(path) = file_path_from_env_or_flag() {
        c.merge_file(parse_file(&read_local(path)?)?)?;  // layer 1 — local file ONLY
    }
    c.merge_env(std::env::vars());            // layer 2
    c.merge_flags(std::env::args());          // layer 3 (highest)
    c.validate()?;                            // §3.3 — before ANY side effect
    Ok(c)
}
```

`merge_*` overwrite only keys the layer actually sets; an unset env var does not clobber a file value. The merge is a field-wise "last writer wins," not a struct replace.

**File source is local-only.** `read_local` rejects any path that is not an absolute or cwd-relative filesystem path. There is no URL scheme handling here — closing the "never read config from the network" door at the type level, not by convention.

### 3.2 The config surface (canonical env/flag table)

Every row is env-settable (12-factor III). Flag overrides env overrides file overrides default.

| Concern | Env | Flag | Notes |
|---|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction "…"` / `@file` | `@file` = ConfigMap/Secret projection |
| Intelligence transport | `AGENTD_INTELLIGENCE` | `--intelligence unix:│https:│vsock:` | see RFC 0006 |
| Intelligence creds | `AGENTD_INTELLIGENCE_TOKEN` | `--intelligence-token` | secret-env or file; **never logged** |
| Model / params | `AGENTD_MODEL`, `AGENTD_MAX_TOKENS` | `--model`, `--max-tokens` | |
| MCP servers | `AGENTD_MCP_CONFIG` (file path) | `--mcp name=cmd`, `--mcp-config` | file = ConfigMap volume |
| Mode | `AGENTD_MODE` | `--mode once│loop│reactive│schedule` | selects exit predicate (RFC 0008) |
| Interval | `AGENTD_INTERVAL` | `--interval` | loop/schedule modes |
| Subscriptions | `AGENTD_SUBSCRIBE` (csv) | repeated `--subscribe URI` | reactive mode (RFC 0008) |
| Serve self-MCP | `AGENTD_SERVE_MCP` | `--serve-mcp unix:…` | opt-in; off for one-shot (RFC 0005) |
| Enable exec | `AGENTD_ENABLE_EXEC` | `--enable-exec [allowlist]` | off by default (RFC 0012) |
| Limits | `AGENTD_MAX_STEPS`/`_MAX_TOKENS`/`_DEADLINE`/`_MAX_DEPTH`/`_TREE_TOKEN_BUDGET` | `--max-steps` etc. | bound the model loop (RFC 0007/0009) |
| **Drain timeout** | `AGENTD_DRAIN_TIMEOUT` | `--drain-timeout` | **MUST be < pod `terminationGracePeriodSeconds`** |
| Log format | `AGENTD_LOG_FORMAT` | `--log-format` | json default in container |
| Log level | `AGENTD_LOG_LEVEL` / `RUST_LOG` | `--log-level` | (RFC 0010) |
| Health file | `AGENTD_HEALTH_FILE` | `--health-file` | exec-probe target (RFC 0010) |
| Health addr | `AGENTD_HEALTH_ADDR` | `--health-addr` | off ⇒ no listener (RFC 0010) |
| **Run ID** | `AGENTD_RUN_ID` | `--run-id` | idempotency key (§6) |
| Cgroup path | `AGENTD_CGROUP` (auto-detect) | `--cgroup` | subagent placement (RFC 0003) |

**The file (`AGENTD_MCP_CONFIG`) carries only verbose structural bits — MCP server lists.** It MUST NOT carry secrets (the validator rejects a `token`/`*_token`/`password`/`secret` key appearing in the file with a hard error, exit `2`) and MUST NOT carry per-environment scalars that belong in env (this is a documented convention, not enforced — env simply wins anyway).

### 3.3 Validate-fully-at-startup → exit 2 in milliseconds, before any side effect

`Config::validate()` runs **after** all four layers merge and **before** the first side effect — no MCP connect, no LLM call, no subagent spawn, no socket bind. It is pure-CPU and sub-millisecond. On the first failure it writes one structured `config.invalid` line to stderr and exits `2` (`EXIT_USAGE`).

```rust
fn validate(&self) -> Result<(), ConfigError> {
    // type/parse validity
    self.intelligence.scheme_supported()?;          // unix/https/vsock; https needs `tls` feature
    self.limits.deadline.ensure_finite()?;          // a deadline is mandatory (assessment §2.8)
    if matches!(self.mode, Mode::Once | Mode::Loop | Mode::Schedule) && self.instruction.is_none() {
        return Err(ConfigError::missing("INSTRUCTION", self.mode));
    }
    if matches!(self.mode, Mode::Reactive) && self.subscribe.is_empty() && self.serve_mcp.is_none() {
        return Err(ConfigError::reactive_needs_event_source());
    }
    // schedule needs a clock source (RFC 0008): an --interval or --cron
    if matches!(self.mode, Mode::Schedule) && self.interval.is_none() && !self.cron_set() {
        return Err(ConfigError::schedule_needs_clock_source());
    }
    reject_secret_keys_in_file(&self.file_keys)?;   // §3.2

    // the cloud-native footgun guard (assessment §2.10, §2.8)
    if let Some(grace) = pod_grace_hint() {          // K8s downward-API env, if injected
        if self.drain_timeout >= grace {
            return Err(ConfigError::drain_exceeds_grace(self.drain_timeout, grace));
        }
    }
    if self.drain_timeout >= Duration::from_secs(30) {
        warn_loud("drain_timeout >= 30s; ensure terminationGracePeriodSeconds is larger");
    }
    Ok(())
}
```

**Drain-vs-grace validation.** `terminationGracePeriodSeconds` is not visible to the process by default. The operator is documented to inject it via the downward API as `AGENTD_POD_GRACE_SECONDS` (or it is passed as `--pod-grace`); when present, `drain_timeout >= grace` is a **hard validation error (exit 2)**. When absent, a `drain_timeout >= 30s` (the K8s default grace) emits a loud `config.warn` line but does not fail — we cannot prove the coupling is wrong, only flag the likely footgun. Default `AGENTD_DRAIN_TIMEOUT=25s` against the recommended `terminationGracePeriodSeconds: 30` leaves headroom for the SIGKILL rung plus log flush.

**Fast-fail rationale:** a config-broken pod must crash in milliseconds with a clear stderr message, so `CrashLoopBackoff` is fast and the failure is unambiguously "operator error" (exit `2` = non-retriable, §5). It must never burn a 30 s LLM round-trip before discovering a typo'd flag.

---

## 4. Mechanisms — signals & drain choreography

### 4.1 Signal handlers (raw `libc::sigaction`, no `SA_RESTART`)

Handlers are installed once at startup via raw `sigaction` (assessment §2.2 — **no `signal-hook`**). `SA_RESTART` is deliberately **off** so a syscall blocked in the reactor returns `EINTR` and the loop observes the flipped flag promptly. Each handler does the minimum async-signal-safe work: flip an `AtomicBool` and write one byte to the self-pipe so the reactor's `recv_timeout`/`poll` wakes immediately (assessment §2.1, §2.2).

```rust
// signals.rs
static DRAINING: AtomicBool = AtomicBool::new(false);  // one-way latch
static FORCE:    AtomicBool = AtomicBool::new(false);  // second-signal force-kill
static REAP:     AtomicBool = AtomicBool::new(false);  // SIGCHLD pending

extern "C" fn on_term_or_int(_sig: c_int) {
    // async-signal-safe only: atomics + write(2) one byte
    if DRAINING.swap(true, Ordering::SeqCst) {
        FORCE.store(true, Ordering::SeqCst);   // already draining => this is the 2nd signal
    }
    let _ = wake_self_pipe();                  // write(self_pipe_w, &[b'!'], 1)
}
extern "C" fn on_sigchld(_sig: c_int) { REAP.store(true, Ordering::SeqCst); let _ = wake_self_pipe(); }

pub fn install() {
    set_handler(SIGTERM, on_term_or_int, /*SA_RESTART=*/false);
    set_handler(SIGINT,  on_term_or_int, /*SA_RESTART=*/false);
    set_handler(SIGCHLD, on_sigchld,     /*SA_RESTART=*/false);
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }   // assessment §2.8 — one line
    // SIGHUP: not handled => default disposition. No reload in v1.
}
```

Signal disposition summary:

| Signal | Disposition | Effect |
|---|---|---|
| `SIGTERM` | handler | 1st → `DRAINING`; 2nd → `FORCE` |
| `SIGINT` | handler | identical to SIGTERM |
| `SIGCHLD` | handler | set `REAP`, wake reactor → `waitpid(-1, WNOHANG)` loop (RFC 0003) |
| `SIGPIPE` | `SIG_IGN` | supervisor survives writing to a just-dead child |
| `SIGHUP` | default | **no reload** — restart-to-reload in v1 (assessment §2.10) |
| `SIGKILL` | (uncatchable) | no handler runs; safety is a property of state, not code (§4.4) |

**Why SIGHUP/reload is dropped:** the retired `signals.rs` mapped SIGHUP→reload. v1 drops it. Config is immutable for a process's lifetime; to change config, restart the pod. This eliminates a whole class of mid-flight reconfiguration bugs (partial re-handshake, subscription churn) and keeps the supervisor's config a validated, frozen snapshot.

### 4.2 The drain state machine

The supervisor runs a single state machine: `RUNNING → DRAINING → EXITING`. The transition to `DRAINING` is **one-way and monotonic** — once draining, the loop never returns to `RUNNING`.

```
                 SIGTERM/SIGINT (1st)
   RUNNING ─────────────────────────────► DRAINING ──────► EXITING ──► exit(code)
      │                                       │   ▲
      │ exit-predicate met (once/loop)        │   │ 2nd signal => FORCE
      └───────────────────────────────────────┘   │ or drain_timeout elapsed
                                                   └────────────► immediate SIGKILL all groups
```

On entering `DRAINING` the reactor runs the bounded drain (assessment §2.10 choreography; ladder mechanics in RFC 0003):

1. **Disarm triggers.** Stop the interval/cron timer; stop routing new `notifications/resources/updated` into spawn/continue (RFC 0008); set the tree-wide `draining` flag so the self-MCP `subagent.spawn` tool returns `-32000 "shutting down"` to any new spawn — including spawns a child attempts mid-teardown (this is the "parent can't spawn replacements mid-teardown" guard from assessment §2.8). Flip readiness to not-ready (RFC 0010).
2. **Wind down in-flight subagents at turn boundaries.** Send each in-flight root subagent a `ctrl:cancel` (cooperative "wind down"); the agentic loop checks the cancel flag at each turn boundary (RFC 0007) and returns a labeled partial. Budget: `min(AGENTD_DRAIN_TIMEOUT, remaining deadline)`.
3. **Ladder the stragglers.** Subagents still alive at the soft deadline go through the bounded **depth-first, deepest-first** kill ladder: `killpg(SIGTERM)` → grace ~5 s → `killpg(SIGKILL)` → `waitpid` until reaped or `ECHILD` (full mechanics in RFC 0003). stdio MCP server children get close-stdin → SIGTERM → SIGKILL (RFC 0004).
4. **Flush logs, exit.** Flush the JSON-lines logger and any gated trace/metric exporter (RFC 0010), then `exit(code)` with the §5 code — **`0` for a clean drain**, not `143`.

The **total drain budget is bounded by `AGENTD_DRAIN_TIMEOUT`** and the per-rung graces sum to less than it; this is why the timeout MUST be smaller than the pod grace (§3.3). If the kubelet's own SIGKILL lands first, we lose the clean exit — so our internal budget is always the smaller number.

### 4.3 Second signal → force

A second `SIGTERM`/`SIGINT` during drain sets `FORCE`. The reactor, on its next wake (immediate, via the self-pipe byte), abandons the graceful rungs and collapses straight to `killpg(SIGKILL)` of **every** tracked process group, then a final `waitpid` sweep, then `exit`. This is the operator's "I'm done waiting" escape hatch and matches the assessment's "second signal → force-SIGKILL."

### 4.4 SIGKILL safety is a property of state, not a handler

No code runs on `SIGKILL`. Safety therefore comes from design, not cleanup:

- The supervisor holds **no durable state** a SIGKILL can corrupt (assessment §2.8 — stateless supervisor; §4 below). Nothing to flush ⇒ nothing to corrupt.
- Orphaned subagents are reaped by `PR_SET_PDEATHSIG, SIGKILL` on every child (immediate-parent chaining) plus cgroup `cgroup.kill` for tree-wide teardown where delegated (RFC 0003). agentd never hard-requires cgroup write access.
- Everything a SIGKILL interrupts mid-flight is recovered by an **idempotent retried re-run** (§6), not by cleanup.

---

## 5. Mechanisms — the exit-code contract

Exit codes are how a `podFailurePolicy` decides retriable vs terminal (`onExitCodes` with `Ignore`/`FailJob`/`Count`). They are partitioned into "do not retry" vs "retry may help." This extends — not reinvents — the retired `runtime.rs` constants (`EXIT_OK=0`, `EXIT_USAGE=2`, `EXIT_SEMANTIC=5`, `EXIT_PAUSED=7`).

### 5.1 The table (reproduced verbatim from assessment §2.10, with scheduler hints)

| Code | Name | Meaning | Scheduler hint |
|---|---|---|---|
| `0` | `EXIT_OK` | success — one-shot completed / loop hit a clean bound / **clean SIGTERM drain** (returns **0, not 143**) | Complete |
| `1` | `EXIT_FAILURE` | generic / unspecified failure not otherwise classified | retriable |
| `2` | `EXIT_USAGE` | config / usage error (validation failed) | **non-retriable** — `FailJob` |
| `3` | `EXIT_PARTIAL` | one-shot produced a *partial* result (useful output emitted; some sub-tasks failed / budget hit mid-work) | policy (default retriable) |
| `4` | `EXIT_INTELLIGENCE` | intelligence endpoint unreachable / auth error after retries | retriable (often transient/upstream) |
| `5` | `EXIT_SEMANTIC` | agent ran correctly but concluded the task *cannot* be done / refused | **non-retriable** — deterministic |
| `6` | `EXIT_MCP` | a required MCP server failed to connect/handshake or died unrecoverably | retriable (sidecar may be racing up) |
| `7` | `EXIT_BUDGET` | hit max-steps / max-tokens / deadline / tree budget before a result | policy (usually raise budget) |
| `124` | `EXIT_TIMEOUT` | hard wall-clock deadline (`--deadline`) tripped — mnemonic to `timeout(1)` | — |
| `137` | `128+SIGKILL` | killed by SIGKILL (OOM, kubelet) — OS-set | OOM ⇒ raise memory limit |
| `143` | `128+SIGTERM` | exited *because of* SIGTERM **without** clean drain (escalated) — OS-set | distinguishes ungraceful from `0` |

```rust
// exit.rs — the public API. Treat any change as breaking.
#[repr(i32)]
pub enum ExitCode {
    Ok = 0, Failure = 1, Usage = 2, Partial = 3, Intelligence = 4,
    Semantic = 5, Mcp = 6, Budget = 7, Timeout = 124,
    // 137/143 are never returned by us; the kernel sets them when it kills us.
}
```

### 5.2 Mapping rules

**One-shot (`once`)** maps the root subagent's terminal status to a code. The `TerminalStatus` enum is owned by **RFC 0007 §3.4** (the authority); this RFC consumes it verbatim and does not introduce variants. The *fatal-infrastructure* aborts (intelligence-unreachable/auth, required-MCP-down) are RFC 0007 §3.6 abort outcomes carried alongside the loop's terminal status, not enum members — they map straight to codes 4/6:

```rust
fn once_exit(outcome: RunOutcome) -> ExitCode {
    match outcome.terminal {
        // RFC 0007 §3.4 terminal-status enum (the closed set):
        TerminalStatus::Completed   => ExitCode::Ok,        // 0
        TerminalStatus::Refused     => ExitCode::Semantic,  // 5  (non-retriable)
        // exhausted_*/deadline/stalled WITH a usable partial → 3; without → as below
        TerminalStatus::ExhaustedSteps
        | TerminalStatus::ExhaustedTokens
            => if outcome.has_usable_partial { ExitCode::Partial } else { ExitCode::Budget }, // 3 | 7
        TerminalStatus::Deadline    => ExitCode::Timeout,   // 124
        TerminalStatus::Stalled
        | TerminalStatus::LoopDetected
            => if outcome.has_usable_partial { ExitCode::Partial } else { ExitCode::Failure }, // 3 | 1
        TerminalStatus::Cancelled   => ExitCode::Ok,        // 0 on a clean drain-cancel (§4.2)
        TerminalStatus::Crashed     => ExitCode::Failure,   // 1
    }
}

// Fatal-infrastructure aborts (RFC 0007 §3.6), surfaced separately from the
// loop's TerminalStatus, short-circuit the mapping:
//   intelligence unreachable / auth after retries → ExitCode::Intelligence (4)
//   required MCP server failed to connect/handshake/died → ExitCode::Mcp (6)
//   tree-wide token ceiling spent (RFC 0003 begin_drain) → ExitCode::Budget (7)
```

This mirrors the retired `Completed ⇒ EXIT_OK, _ ⇒ EXIT_SEMANTIC` logic but with finer partitioning so a `podFailurePolicy` can branch (refused vs partial vs budget vs timeout are now distinct). "Partial" (exit 3) is **not** a `TerminalStatus`: it is any `exhausted_*`/`deadline`/`stalled`/`loop_detected` outcome that nonetheless emitted a usable partial result (RFC 0007 §4) — the supervisor decides it from the result body, not from a distinct status.

**loop / reactive daemons** never exit on an individual task failure — a failed reaction is logged and the daemon keeps serving (the restart governor handles crash-looping children, RFC 0003). They exit **only**:

- `0` — clean drain on SIGTERM, or (loop) a clean bound hit;
- `143` — SIGTERM forced past the drain budget (kernel-set, ungraceful);
- a fatal class — `4` (intelligence gone), `6` (required MCP gone), `137` (OOM-killed).

A reactive `Deployment` rolled by the operator must therefore look like a **clean `0`** in dashboards, not a `143` failure — which is precisely why a successful drain returns `0`.

**`EXIT_PARTIAL`/`EXIT_BUDGET` default retriable-policy is operator-tunable.** "Raise the budget" vs "retry" is deployment-specific; a `--budget-exit-code` flag lets an operator remap `7` (and `3`) to a code their `podFailurePolicy` treats as terminal. Default behavior is the table above. (This resolves the open question in `notes-review-cloud-native-unit.md` §12 by making it policy, not a fixed verdict.)

**The exit-code list is documented in `--help` and in `docs/`** so operators can author `podFailurePolicy` rules against it. It is a public API; changes are breaking.

---

## 6. Mechanisms — idempotency

A scheduler retries (`backoffLimit`, exponential backoff, at-least-once). A one-shot run must therefore be **safe to execute more than once.** `agentd` cannot *make* an arbitrary instruction idempotent — but it provides the mechanism and introduces no non-idempotency of its own.

### 6.1 `RUN_ID` — the idempotency key

```rust
// resolved in config.rs
let run_id: Ulid = env_or_flag("AGENTD_RUN_ID", "--run-id")
    .map(Ulid::from_str).transpose()?     // operator-supplied stable key
    .unwrap_or_else(Ulid::new);           // default: per-process random ULID
```

- **Default** is a per-process random ULID — so logs/traces correlate across the tree (it is the `run_id` field in the log schema, RFC 0010), but a default does *not* dedupe retries (each retry mints a new one).
- **For retry-dedupe the operator sets a stable key** per logical unit of work (e.g. K8s injects the Job name or a hash of the input). Same logical work → same `RUN_ID` across retries.

### 6.2 Propagation into every MCP `_meta`

`RUN_ID` is injected into the `_meta` of **every** outbound MCP `tools/call` (and `resources/read` where a backing service uses it). This rides alongside the W3C trace-context already placed in `_meta` (RFC 0010 / assessment §2.9). MCP's `_meta` is in-spec for arbitrary request metadata; using it for an idempotency key is a documented convention the backing server must honour.

```jsonc
// tools/call request shape (RFC 0004 codec), _meta carries the idempotency key
{
  "jsonrpc": "2.0", "id": 42, "method": "tools/call",
  "params": {
    "name": "queue.enqueue",
    "arguments": { "topic": "digests", "body": "…" },
    "_meta": {
      "agentd/run_id": "01J8Z3K2Qn7…",          // the idempotency key
      "traceparent": "00-<trace_id>-<span_id>-01" // trace-context (RFC 0010)
    }
  }
}
```

A backing service that supports idempotency keys (a queue with dedupe, an HTTP API honouring `Idempotency-Key`) reads `agentd/run_id` from `_meta` and collapses a retried side effect to a single effect. This is the only hook `agentd` needs to offer; the dedupe lives in the backing service, by design (assessment §2.11).

### 6.3 Read-modify-write through MCP; "already done" is cheap

The default system prompt and docs encourage the level-triggered pattern: the agent checks current state via `resources/read` before mutating, so a re-run that finds work already done is a no-op. A re-run of an already-complete unit should **detect "already done" via the backing service and exit `0` immediately and cheaply** — not redo LLM work. This makes retries cheap and safe, and is the same reconcile-to-desired-state pattern that governs reactive restart (RFC 0008's read-after-subscribe).

### 6.4 No local non-idempotent side effects — structural

`agentd` itself writes nothing durable locally except logs (stdout = the agent's result; stderr = telemetry — both append-only event streams, harmless to duplicate, 12-factor XI). **All durable output goes through MCP backing services** where the idempotency key can act. This is not a discipline we must police — it falls out structurally from the assessment's "**no built-in tools**" decision (§2.11): the only way `agentd` can persist anything is to call an MCP server, which is by construction an external backing service. There is no local file write, no local DB, no hidden side effect path for a retry to duplicate.

**Honest scope statement.** True idempotency is a property of *the instruction + the MCP tools it uses*, which `agentd` does not own. Our contract is exactly three guarantees: (1) provide and propagate a stable idempotency key; (2) introduce no non-idempotent local side effects; (3) make "already done" cheap to detect and exit `0` on. Beyond that, idempotency is the operator's composition responsibility — consistent with "composition is MCP, not a control plane we own" (assessment §2.11).

---

## 7. Statelessness & the two deploy shapes of one binary

The supervisor is **stateless and share-nothing** (12-factor VI). State classification on restart:

| State | Lives | On restart |
|---|---|---|
| Config | env/flags/file (external) | re-read from environment |
| MCP connections | in-memory, reconstructable | re-established (re-handshake) |
| Declared subscriptions | config | re-subscribed from config |
| Dynamic subscriptions (self-MCP `subscribe`) | in-memory | lost in v1 |
| Warm reactive sessions | in-memory | lost in v1 (recovered by idempotent re-trigger) |
| Subagent process tree | OS processes | gone (died with the pod) |
| Final results / outputs | externalized through MCP | durable — the point |

Because durable output is externalized (§6.4) and reactions are idempotent (§6), a **daemon restart is equivalent to a cold start that reconciles**: re-read config → re-handshake MCP → re-subscribe every declared subscription → **read-after-subscribe** each to convert edge-triggering to level-triggering across the restart boundary (the mandatory reconcile rule lives in assessment §2.8 and RFC 0008). This makes a reactive daemon restart-safe with **no persistence layer**. Local-disk session files as durable state are **explicitly rejected** (a pod reschedules to a new node; container FS is ephemeral; durable = a backing service over MCP). The optional MCP-backed warm-session checkpoint is deferred to v2 (RFC 0013).

The two deploy shapes are thus the **same binary, same loop, same config machinery**, differing only in the exit predicate (assessment §2.6, RFC 0008) and therefore in which exit codes are reachable:

- **Unit of work** (`once`, CLI/`Job`/`CronJob`): event source is "the instruction"; goes empty-and-final; reachable codes `0/1/3/4/5/6/7/124` (+ kernel `137`).
- **Reactive daemon** (`reactive`, `Deployment`): event source is "an unbounded subscription stream"; never empty-and-final; reachable codes `0/143` + fatal `4/6/137`.

`loop` straddles: a bounded `Job`-with-deadline (one-shot-like codes) or a `Deployment` (daemon-like codes). One binary, two shapes, one contract.

---

## 8. Interactions with other RFCs

- **RFC 0001 (core):** this RFC realizes the cloud-native deployment shapes the core thesis describes; the "no built-in tools" decision is what makes §6.4 structural.
- **RFC 0003 (supervision / dead-stuck / recovery):** owns the kill-ladder mechanics, `PR_SET_PDEATHSIG`, `PR_SET_CHILD_SUBREAPER`, the `SIGCHLD`→`waitpid` reap loop, cgroup awareness, and rebuild+reconcile. §4 invokes the ladder and §7 invokes reconcile; the *signal entry points and drain budget* are owned here.
- **RFC 0004 (MCP client):** the `_meta` injection point (§6.2) rides the same outbound-request path; the stdio MCP-server child shutdown ladder (close-stdin → SIGTERM → SIGKILL) is part of §4.2 step 3.
- **RFC 0005 (self-MCP server):** the `draining` flag makes `subagent.spawn` reject new spawns during drain (§4.2 step 1).
- **RFC 0006 (intelligence):** `EXIT_INTELLIGENCE` (4) and the auth/unreachable terminal statuses map here; credential resolution feeds the `Secret` config fields.
- **RFC 0007 (agentic loop):** the terminal statuses §5.2 maps to exit codes are defined there; the per-turn cancel check that makes drain-at-turn-boundary work is its loop invariant.
- **RFC 0008 (modes / routing):** owns the exit predicates that determine which codes are reachable (§7) and the read-after-subscribe reconcile invoked on restart.
- **RFC 0009 (subagents):** spawn-chokepoint caps; the `draining` flag also blocks the spawn chokepoint.
- **RFC 0010 (observability / health):** owns the health file/`/healthz`/`/readyz` surface and the log schema (`run_id`, the `config.*`/`proc.*`/`drain.*` events). This RFC owns *when* readiness flips on drain (§4.2 step 1) but not the surface itself.
- **RFC 0012 (security):** secrets are env/flag-only and never in the config file (§3.2); `exec` is off by default.
- **RFC 0013 (deferred v2):** the MCP-backed warm-session checkpoint (§7).

---

## 9. Non-goals / Deferred

- **No internal robust scheduler.** Time-scheduling is **external by default** (a `CronJob` firing `--mode once` per tick is more robust, observable, and 12-factor). The internal `--interval`/`cron` is a standalone convenience fed into the same reactive router (assessment §2.6, RFC 0008) — no calendar/DST/missed-tick-catch-up/job-store; default TZ UTC.
- **No SIGHUP/reload, no live reconfiguration.** Restart-to-reload in v1 (§4.1). Config is a frozen, validated snapshot for a process's lifetime.
- **No remote/network config.** All input is env + flags + a local file (§3.1).
- **No warm-session checkpointing in v1.** Restart = rebuild + reconcile (§7); the optional MCP-backed checkpoint is deferred to RFC 0013.
- **`agentd` does not guarantee instruction-level idempotency.** It provides the key, propagates it, and adds no local non-idempotency (§6.4); end-to-end idempotency is the operator's MCP composition.
- **cgroup write access is never required** (degrade to rlimit + PDEATHSIG; mechanics in RFC 0003).
- **The health surface, log schema, and trace propagation** are RFC 0010, referenced not re-specified.

---

## 10. Open items

- **Downward-API grace hint key.** §3.3 assumes the operator injects `terminationGracePeriodSeconds` as `AGENTD_POD_GRACE_SECONDS` (or `--pod-grace`) so the drain-vs-grace check can be a hard error rather than a warning. The exact env-var name is a documentation convention to settle in M5; if unset we fall back to the `>= 30s` warning. This is a naming convention, not a design gap.
- **`agentd/run_id` `_meta` key namespace.** §6.2 uses `agentd/run_id`; whether to additionally mirror it as a conventional `Idempotency-Key` for HTTP-bridging MCP servers (so they need no agentd-specific awareness) is a small documented convention to confirm with the first backing-service integration. Does not block M5.
