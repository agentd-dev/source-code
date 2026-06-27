# RFC 0017: Declarative configuration & hot reload — the config file, validation, and restart-free reconfiguration

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentd rewrite — control-plane track (RFC 0014); extends the cloud-native contract (RFC 0011) — specifically its config-precedence, validate-at-startup, signal, and secrets-out-of-the-file rules.

---

## 1. Problem / Context

RFC 0011 §2.1 names a **config file layer** in the precedence chain
(`built-in default < config file < env var < CLI flag`) but ships only the
narrow `AGENTD_MCP_CONFIG` MCP-server list as its concrete instance, and RFC 0013
records the broader declarative-config surface as a *deferred* line. Two things
have since changed the calculus, both from RFC 0014:

1. **agentctl exists as a control plane.** It drives a *fleet* from a Kubernetes
   `ConfigMap`/CR, must validate a desired-config object **before** it is applied
   (an admission webhook, a CI gate), and must reconfigure a running instance
   **without a pod restart** where the change is reloadable. None of those are
   possible against the v1 surface: there is no full file schema, no
   `--validate-config`, no schema export, and RFC 0011 §2.2 explicitly **drops
   SIGHUP** ("restart-to-reload in v1").

2. **The minimalism boundary is now codified** (RFC 0014 §3): agentd exposes
   *primitives* (a file to read, a config to validate, a schema to emit, a signal
   to honour); agentctl owns *policy* (the CRD, the operator reconcile loop, the
   admission webhook wiring, the rollout strategy). This RFC supplies exactly the
   primitives and nothing of the policy.

This RFC therefore (a) lands the full **declarative config file** the RFC 0011
precedence chain always reserved a slot for — verbose *structural* config only,
**never** secrets or per-environment values; (b) adds **`agentd
--validate-config`** and **`agentd --config-schema`** so an external admission
gate can reject a bad config in milliseconds and validate a CR against a JSON
Schema before applying; and (c) **revisits RFC 0011 §2.2's SIGHUP drop** to
specify a *bounded, validate-first, quiesce-and-reapply* **hot reload** of a
precisely-delimited reloadable subset, plus **file-based secret refs** for
rotation-friendly Kubernetes Secret volumes.

This RFC does **not** own: the config-precedence rule itself, the
validate-at-startup discipline, the signal table, the drain choreography, or the
secrets-never-in-the-file rule (all **RFC 0011** / **RFC 0012**). It *extends*
them. It does **not** own the kill/quiesce ladder mechanics (**RFC 0003**), the
terminal-status vocabulary (**RFC 0007 §3.4**), the exit-code table (**RFC 0011
§5**), the MCP wire/codec (**RFC 0004**), the self-MCP surface (**RFC 0005**), or
the event/metric/health surface (**RFC 0010**). It references and reuses each.

**The minimalism moat is non-negotiable.** Everything here is `serde` +
`serde_json` + `libc` in the default build. The file parser is JSON (the
`serde_json` already linked); an *optional* YAML reader is the **only** new
dependency this RFC contemplates and it is feature-gated and off by default (§3.2,
§9). The file-watch path is `libc` (`inotify` raw syscalls) behind a feature gate;
the default reload trigger is `SIGHUP`, which costs nothing. Nothing here pulls an
async runtime, a Kubernetes client, or a TLS/gRPC stack — those are agentctl's, by
construction (RFC 0014 §3, §6).

---

## 2. Decision

1. **A full declarative config file lands the RFC 0011 §2.1 file layer.** A single
   JSON document (optionally YAML behind the `config-yaml` feature) holds **only
   verbose structural config**: the MCP-server list (`name`/`command`/`argv`/`env`
   passthrough names/`tags`/transport), declared subscriptions, the model and
   limits, and the reloadable knobs. It slots into the existing precedence
   (`built-in < file < env < flag`) **unchanged** (RFC 0011 §3.1) and is a
   one-to-one structural superset of today's repeatable `--mcp` / `--mcp-tags` /
   `--subscribe` flags (§3.3). It is sourced **local-only** (`read_local`, RFC
   0011 §3.1 — never the network). It **MUST NOT** contain secrets or
   per-environment scalars; the validator rejects secret-shaped keys with a hard
   error (RFC 0011 §3.2, RFC 0012 §3.7), and per-environment scalars are
   documented to stay in env/flag (env wins anyway).

2. **`agentd --validate-config` is the admission primitive.** It loads and *fully*
   resolves the config (file + env + flags), runs the complete RFC 0011 §3.3
   `Config::validate()` plus this RFC's reload-coherence checks, and exits **0 if
   valid** or **2 with structured diagnostics on stderr** if not — **before any
   side effect** (no MCP connect, no LLM call, no socket bind). It is pure-CPU and
   sub-millisecond. It is the backend an agentctl admission webhook / CI gate
   calls.

3. **`agentd --config-schema` emits the JSON Schema (Draft 2020-12) of the config
   *file* to stdout and exits 0.** The schema is generated from the same Rust types
   the loader deserializes (single source of truth), carries the
   `contract_version` from the capabilities manifest (RFC 0014 §5), and lets
   agentctl validate a CR's embedded config against agentd's own schema *before*
   it ever reaches a pod. The schema is a **frozen, versioned public API** (RFC
   0014 §3 principle 4): additive within a major, breaking changes bump the major.

4. **Hot reload is reinstated for a precisely-delimited reloadable subset, at a
   safe boundary, validate-first, all-or-nothing.** `SIGHUP` (default) or an
   optional `inotify` file-watch (`--watch-config`, `config-watch` feature)
   triggers a reload of: **MCP servers, declared subscriptions, model + intel
   params, limits, log level, and the reloadable timing knobs.** **Restart-only**
   (a reload that touches them is *rejected*, not partially applied): **mode,
   intelligence transport/endpoint identity, instance identity, `--serve-mcp`
   transport, `--enable-exec`, and the run-id.** The reload **re-validates the new
   resolved config first**; on any error it **keeps the old config verbatim and
   logs** — never a half-apply. It runs at a **quiesce boundary** (between agentic
   turns / at the reactive idle point), reusing the drain/quiesce machinery (RFC
   0011 §4.2, RFC 0003 §3.5) to preserve in-flight work where possible. This is the
   conscious, scoped reversal of RFC 0011 §2.2's blanket "SIGHUP dropped."

5. **File-based secret refs land for rotation.** `--intelligence-token-file
   <path>` resolves the intelligence credential from a mounted file, and the
   existing `{{secret:NAME}}` interpolation (RFC 0006 §3 / RFC 0012 §3.7) gains a
   sibling **`{{secret-file:PATH}}`** that reads a mounted Secret-volume file at
   the moment of use. Secrets are **still** sourced env/file only, **never** enter
   the config file, **never** enter a log/transcript/checkpoint, and `Secret`'s
   `Debug`/`Display` stay `***` (RFC 0012 §3.7). File-based refs are re-read on use
   so a Kubernetes Secret rotation takes effect without a restart.

6. **Failure semantics are uniform: validate-before-apply, always; a bad reload is
   a no-op + error event; a precedence conflict resolves by the RFC 0011 rule and
   is logged, never errored.** No path half-applies config. (§7.)

These decisions are **additive and feature-gated**; a default `agentd` build that
ships none of the control-plane surfaces reports `hot_reload:false` in its manifest
(RFC 0014 §5) and behaves exactly as RFC 0011 specifies (restart-to-reload).

---

## 3. Mechanisms — the declarative config file

### 3.1 What the file is *for* (and pointedly not for)

The file carries the config that is **verbose, structural, and environment-stable**
— the parts that are painful to express as a wall of repeated flags and that do
**not** vary per environment or per replica. Concretely: the MCP-server inventory,
the declared subscription set, the model/limits/reloadable knobs.

The file is **not** for:

- **Secrets.** Never. The validator hard-rejects a `token` / `*_token` /
  `password` / `secret` / `*_key` key appearing in the file (RFC 0011 §3.2, exit
  `2`). Credentials are env/flag/file-ref only (§6, RFC 0012 §3.7).
- **Per-environment scalars** (the intelligence endpoint, the namespace, the
  pod/instance identity, the run-id). These belong in env (12-factor III). This is
  a documented convention rather than a hard error — env wins over the file anyway
  by precedence (RFC 0011 §3.1), so putting a per-env scalar in the file is merely
  pointless, not unsafe. The exception is identity-class fields the validator
  *does* flag if file-set (§5 restart-only set), because file-setting them implies
  a misunderstanding worth surfacing.

The boundary is exactly RFC 0011 §3.2's: *"the file carries only verbose
structural bits."* This RFC widens *which* structural bits, never the *kind*.

### 3.2 Source, format, precedence

- **Source.** `--config <path>` (and `AGENTD_CONFIG=<path>`) name the file;
  `AGENTD_MCP_CONFIG` (RFC 0011 §3.2) remains a recognized alias whose document is
  the `mcp_servers` sub-object only, for back-compat. Resolution is **local-only**
  via `read_local` (RFC 0011 §3.1) — an absolute or cwd-relative filesystem path;
  no URL scheme. "Never read config from the network" is closed at the type level,
  unchanged.
- **Format.** **JSON by default** (`serde_json`, already linked — zero new
  dependency; JSON-with-comments tolerated by stripping `//`/`/* */` before parse,
  matching the jsonc shown throughout this set). YAML is supported **only** when the
  binary is built with the `config-yaml` feature, which links a single small YAML
  reader; it is **off in the default and cloud-native image builds** (§9). The
  parser is selected by extension (`.yaml`/`.yml` → YAML iff the feature is on,
  else a config error), so the moat holds: a default build cannot be handed YAML.
- **Precedence.** Exactly RFC 0011 §3.1: the file merges as **layer 1**, between
  built-in defaults and env. `merge_file` overwrites only the keys the file
  actually sets (field-wise last-writer-wins). An env var or flag for the same key
  wins. For the **list-valued** keys (`mcp_servers`, `subscribe`) the merge is
  **replace, not append** at the layer boundary, but env/flag *additions* compose:
  `--mcp` flags and `--subscribe` flags **add to** the file's list rather than
  replacing it (this matches the repeatable-flag semantics operators already
  expect, and is the one documented deviation from pure last-writer-wins — §3.3).

### 3.3 Shape, and the exact map to today's flags

The file is a structural superset of the repeatable `--mcp` / `--mcp-tags` /
`--subscribe` flags (RFC 0011 §3.2) and the RFC 0012 §3.1 tag wire. Canonical
shape:

```jsonc
{
  // optional; pins the file to a schema major agentctl validated against.
  "config_version": "1.0",

  // ── reloadable: model + intel params (NOT the transport/endpoint — §5) ──
  "model": "claude-opus-4",
  "max_tokens": 2000000,

  // ── reloadable: bounds on the model loop (RFC 0007/0009) ──
  "limits": {
    "max_steps": 200,
    "max_depth": 4,
    "deadline_secs": 600,
    "tree_token_budget": 8000000,
    "max_total_subagents": 64
  },

  // ── reloadable: the MCP server inventory ──
  // one object per server == one `--mcp name=cmd … --mcp-tags …` flag group.
  "mcp_servers": [
    {
      "name": "web",                         // == --mcp web=…
      "command": "mcp-fetch",                // argv[0]
      "argv": ["--timeout", "30"],           // argv[1..]
      "transport": "stdio",                  // stdio (default) | unix
      "env_passthrough": ["HTTP_PROXY"],     // names only — values from process env, never inline
      "tags": { "*": ["untrusted_input"] }   // RFC 0012 §3.1 glob→tags; untagged ⇒ untrusted_input
    },
    {
      "name": "vault",
      "command": "mcp-vault",
      "tags": { "read_*": ["sensitive"], "write_*": ["sensitive", "egress"] }
    }
  ],

  // ── reloadable: declared subscriptions (reactive mode, RFC 0008) ──
  // each string == one `--subscribe URI`.
  "subscribe": [
    "fs:file:///watch/inbox",
    "queue:agentd://topic/digests"
  ],

  // ── reloadable: observability knobs that are safe to change live ──
  "log_level": "info",                       // == --log-level / AGENTD_LOG_LEVEL

  // ── reloadable: declared intelligence HTTP headers (RFC 0006 §3) ──
  // values MAY interpolate {{secret:NAME}} / {{secret-file:PATH}} (§6).
  // the NAMES/refs are structural; the resolved secret never lands here or in logs.
  "intelligence_headers": {
    "anthropic-version": "2023-06-01",
    "x-api-key": "{{secret:ANTHROPIC_API_KEY}}",
    "authorization": "Bearer {{secret-file:/var/run/secrets/intel/token}}"
  }
}
```

Field-to-flag equivalence (the file is never *more* expressive than the flags; it
is the same surface, organized):

| File key | Equivalent flag(s) (RFC 0011 §3.2) | Reloadable (§5) |
|---|---|---|
| `model`, `max_tokens` | `--model`, `--max-tokens` | yes |
| `limits.*` | `--max-steps` / `--max-depth` / `--deadline` / `--tree-token-budget` / … | yes |
| `mcp_servers[]` | repeated `--mcp name=cmd`, `--mcp-tags`, `--mcp-config` | yes |
| `subscribe[]` | repeated `--subscribe URI` | yes |
| `log_level` | `--log-level` / `AGENTD_LOG_LEVEL` | yes (live) |
| `intelligence_headers` | the declared-header set (RFC 0006 §3) | yes |
| *(absent — restart-only)* | `--mode`, `--intelligence`, `--serve-mcp`, `--enable-exec`, `--run-id` | **no** (§5) |

The restart-only fields are **deliberately not first-class file keys** — putting
them in the file is a validation warning (§5), because their natural home is env/
flag (per-environment, identity, transport). This keeps the file *structural* and
the per-environment surface in env, exactly as RFC 0011 intends.

```rust
// config_file.rs — the deserialized shape. One source of truth for the loader,
// the validator, and the --config-schema generator (§4.2). serde only.
#[derive(serde::Deserialize, schema::Describe)]   // Describe = our tiny in-tree derive (§4.2)
#[serde(deny_unknown_fields)]                       // a typo'd key is exit 2, not silently ignored
pub struct ConfigFile {
    pub config_version: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub limits: Option<LimitsFile>,
    #[serde(default)] pub mcp_servers: Vec<McpServerFile>,
    #[serde(default)] pub subscribe: Vec<String>,
    pub log_level: Option<Level>,
    #[serde(default)] pub intelligence_headers: BTreeMap<String, String>,
    // a flattened catch-all is INTENTIONALLY ABSENT — deny_unknown_fields is the guard.
}
```

`deny_unknown_fields` makes a typo'd key (`max_token` vs `max_tokens`) a hard
config error (exit `2`) instead of a silently-ignored value — the single most
common config footgun, closed at parse time.

---

## 4. Mechanisms — validation & schema export

### 4.1 `agentd --validate-config` — the admission primitive

`--validate-config` is the RFC 0011 §3.3 `Config::validate()` pipeline run as a
standalone, side-effect-free command:

```
agentd --validate-config [--config FILE]      # plus any env/flags to resolve against
  → load (built-in < file < env < flag)        # RFC 0011 §3.1 — same loader, no shortcuts
  → Config::validate()                          # RFC 0011 §3.3 — the full check
  → reload_coherence_check()                    # §5.4 — this RFC's additional checks
  → exit 0  (one line: {"event":"config.valid"} to stderr)
  | exit 2  (one or more {"event":"config.invalid", …} diagnostics to stderr)
```

It is **pure-CPU, sub-millisecond, and performs no side effect** — no MCP
connect, no LLM call, no socket bind, no file-watch arm. It resolves the *same*
four layers the daemon would, so what `--validate-config` accepts is exactly what
the daemon will accept (no drift between the gate and the runtime). On the first
*and every subsequent* failure it emits a structured `config.invalid` line; unlike
the startup path (which fast-fails on the first error, RFC 0011 §3.3), the
*validate* command **collects and emits all diagnostics** before exiting `2`, so an
operator/CI sees every problem in one pass:

```jsonc
// stderr, exit 2 — machine-actionable for an admission webhook
{"event":"config.invalid","ts":"…","path":"mcp_servers[1].tags","code":"unknown_tag",
 "msg":"tag 'sensitiv' is not one of untrusted_input|sensitive|egress","value":"sensitiv"}
{"event":"config.invalid","ts":"…","path":"limits.deadline_secs","code":"missing",
 "msg":"a finite deadline is mandatory (RFC 0011 §3.3)"}
{"event":"config.invalid","ts":"…","path":"$","code":"secret_in_file",
 "msg":"key 'x-api-key' has an inline secret-shaped value; use {{secret:NAME}} or env"}
```

**agentctl usage (policy, not in agentd).** agentctl's admission webhook shells the
config object into `agentd --validate-config` (or links the same crate) and maps
exit `2` → admission *deny* with the diagnostics surfaced to the `kubectl apply`
caller. The reconcile loop runs it as a pre-flight before it rolls a new
`ConfigMap`. agentd ships the verdict; agentctl owns the wiring.

The check set `--validate-config` runs is exactly the union of:

- **RFC 0011 §3.3** (the authority): scheme support, mandatory finite deadline,
  mode↔required-field coherence (instruction for once/loop/schedule; an event
  source for reactive; a clock for schedule), the secret-key-in-file rejection, the
  drain-vs-grace footgun guard.
- **RFC 0012 §3.x** security coherence already wired into startup: a missing
  `--enable-exec` binary, an unresolvable secret ref, `--allow-trifecta` required
  for a trifecta-only grant.
- **This RFC §5.4:** reload-coherence (restart-only fields not file-set without a
  warning; the reloadable subset is internally consistent).

### 4.2 `agentd --config-schema` — the JSON Schema export

`--config-schema` emits the **JSON Schema (Draft 2020-12)** of the config *file*
to stdout and exits `0`. The schema is **generated from the `ConfigFile` Rust
types** by a tiny in-tree derive (`schema::Describe`) — **no `schemars` or other
schema crate** (that would be binary weight and a dependency; the moat forbids it).
The derive walks the same `#[derive(Deserialize)]` structs and emits a `serde_json`
`Value`, so the schema **cannot drift** from what the loader accepts: one source of
truth, deserializer and schema generated from the same type.

```jsonc
// `agentd --config-schema` → stdout (abridged)
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://agentd.dev/schema/config/1.0",
  "x-agentd-contract-version": "1.0",          // ties to the manifest (RFC 0014 §5)
  "type": "object",
  "additionalProperties": false,               // mirrors deny_unknown_fields (§3.3)
  "properties": {
    "config_version": { "type": "string" },
    "model": { "type": "string" },
    "max_tokens": { "type": "integer", "minimum": 1 },
    "limits": { "$ref": "#/$defs/Limits" },
    "mcp_servers": { "type": "array", "items": { "$ref": "#/$defs/McpServer" } },
    "subscribe": { "type": "array", "items": { "type": "string" } },
    "log_level": { "enum": ["error","warn","info","debug","trace"] },
    "intelligence_headers": { "type": "object", "additionalProperties": { "type": "string" } }
  },
  "$defs": {
    "McpServer": {
      "type": "object", "additionalProperties": false,
      "required": ["name", "command"],
      "properties": {
        "name": { "type": "string", "pattern": "^[a-zA-Z0-9_-]+$" },
        "command": { "type": "string" },
        "argv": { "type": "array", "items": { "type": "string" } },
        "transport": { "enum": ["stdio", "unix"] },
        "env_passthrough": { "type": "array", "items": { "type": "string" } },
        "tags": {
          "type": "object",
          "additionalProperties": {
            "type": "array",
            "items": { "enum": ["untrusted_input", "sensitive", "egress"] }
          }
        }
      }
    },
    "Limits": { "type": "object", "additionalProperties": false, "properties": { /* … */ } }
  }
}
```

**Freeze + version (RFC 0014 §3 principle 4).** The schema is a public API
agentctl couples to. It carries `x-agentd-contract-version` (the manifest's
`contract_version`, RFC 0014 §5) and `$id` pins the major. Changes are **additive
within a major** (new optional fields, widened enums in a back-compatible
direction); any breaking change (removing/renaming a field, narrowing an enum,
making an optional field required) bumps the major. agentctl validates a CR against
the schema whose major matches the instance's manifest, and refuses to drive an
instance whose schema major it does not understand.

`--config-schema` is also exposed as a **live self-MCP resource**,
`agentd://config/schema` (RFC 0005 §3.3, `mimeType:"application/schema+json"`), so
agentctl can fetch the schema *from a running instance* it is already managing over
vsock/unix, not just from a one-shot exec — same single-source-of-truth bytes.
Alongside it, `agentd://config/effective` exposes the **resolved, redacted**
running config (secrets shown as `***`, file/env/flag provenance per key) so
`kubectl agent <x> describe` can show what an instance is *actually* running. Both
resources are read-only and emit `notifications/resources/updated` on a successful
reload (§5.3).

---

## 5. Mechanisms — hot reload

### 5.1 What is reloadable vs restart-only (the binding partition)

RFC 0011 §2.2 dropped SIGHUP wholesale to "eliminate a whole class of mid-flight
reconfiguration bugs (partial re-handshake, subscription churn)." This RFC
reinstates reload **only for the subset where re-applying at a quiesce boundary is
provably safe**, and keeps everything whose change would alter process identity,
transport, or the exit-predicate **restart-only**. The partition is binding:

| Field | Class | Why |
|---|---|---|
| `mcp_servers[]` (add/remove/edit a server) | **reloadable** | re-handshake at a quiesce boundary; RFC 0003 §3.11 rebuild machinery already does this on restart |
| `subscribe[]` (add/remove a subscription) | **reloadable** | re-subscribe + **read-after-subscribe** (RFC 0003 §3.11) on add; unsubscribe on remove |
| `model`, `max_tokens`, `intelligence_headers` | **reloadable** | next intel call uses the new value; no in-flight corruption |
| `limits.*` | **reloadable** | applied to **new** spawns/turns; in-flight children keep their minted budgets (§5.5) |
| `log_level` | **reloadable (live)** | applied immediately, no quiesce needed — it gates emission only |
| **`mode`** (once/loop/reactive/schedule) | **restart-only** | changes the **exit predicate** (RFC 0008 / RFC 0011 §7) — the whole shape of the process |
| **intelligence transport / endpoint identity** (`--intelligence` scheme+target) | **restart-only here** | endpoint *resilience* (failover, hot-swap) is **RFC 0018**, not this RFC; a transport change is a restart |
| **instance identity** (`--run-id`, downward-API identity) | **restart-only** | the idempotency key (RFC 0011 §6) and log-correlation root must be stable for a process's life |
| **`--serve-mcp` transport** (stdio/unix/vsock + path/port) | **restart-only** | rebinding a live control socket mid-flight breaks agentctl's connection (RFC 0014/0015) |
| **`--enable-exec`** (and its allowlist) | **restart-only** | a security-capability toggle; never widen the trifecta surface live (RFC 0012 §3.6) |
| **drain timeout / pod-grace coupling** | **restart-only** | the drain budget is validated against `terminationGracePeriodSeconds` at startup (RFC 0011 §3.3); changing it live could break the grace invariant |

A reload whose diff touches **any** restart-only field is **rejected** — the whole
reload is a no-op, the old config stays, and a `config.reload_rejected` event names
the offending field(s) (§5.3, §7). agentctl, seeing that event (or by diffing the
desired vs `agentd://config/effective`), performs a **rolling restart** for those
fields — its policy, not agentd's.

### 5.2 Reload triggers

- **`SIGHUP` (default, zero-dependency).** Installed via raw `sigaction`, `SA_RESTART`
  off, the handler async-signal-safe: it sets a `RELOAD: AtomicBool` and writes one
  byte to the self-pipe (exactly the RFC 0011 §4.1 / RFC 0003 §3.1 pattern). This
  is the conscious change to the RFC 0011 §4.1 signal table:

  | Signal | RFC 0011 disposition | **This RFC** |
  |---|---|---|
  | `SIGHUP` | default (no reload) | **handler → set `RELOAD`, wake reactor** (when `hot-reload` feature on); default disposition when off |

  `SIGTERM`/`SIGINT`/`SIGCHLD`/`SIGPIPE` are **unchanged** (RFC 0011 §4.1). A
  `SIGHUP` received *while `DRAINING`* is ignored (drain wins; reload is meaningless
  on a process that is exiting).

- **File-watch (opt-in, `--watch-config`, `config-watch` feature).** A raw `inotify`
  watch (`libc`, no `notify` crate) on the config file's directory; on a
  `IN_CLOSE_WRITE`/`IN_MOVED_TO` for the watched path it sets the same `RELOAD`
  flag. This makes a `ConfigMap` volume update (Kubernetes atomically swaps the
  symlinked directory) trigger a reload with no signal plumbing. It is **off by
  default**; SIGHUP is the portable, dependency-free default. Both triggers funnel
  into the identical reload routine (§5.3), so there is one code path.

### 5.3 The reload choreography (validate-first, quiesce, all-or-nothing)

On the reactor's next wake after `RELOAD` is set, the supervisor runs a bounded,
ordered routine. It **never** mutates live config until the new config is fully
validated, and it **never** half-applies.

```
1. RE-LOAD + RE-VALIDATE (pure-CPU, no side effect)
   ├─ re-read the config file (local-only), re-merge built-in<file<env<flag.
   │    note: env/flags are FIXED for a process's life; only the FILE can change.
   │    re-merging keeps precedence correct (a flag still overrides the new file).
   ├─ Config::validate()  +  reload_coherence_check()      # §4.1 — the full check
   └─ on ANY error → ABORT: keep old config verbatim,
        emit {"event":"config.reload_rejected","reason":…,"diagnostics":[…]} (warn),
        bump agentd_config_reload_total{result="rejected"}.  DONE. (no-op — §7)

2. DIFF + GATE on the restart-only set (§5.1)
   └─ if the new vs running diff touches any restart-only field →
        ABORT as in (1) with reason="restart_required", field=<name>.
        the OLD config keeps running; agentctl restarts the pod (its policy).

3. QUIESCE to a safe boundary (reuse RFC 0011 §4.2 / RFC 0003 §3.5 machinery)
   ├─ set a tree-wide `reloading` flag so subagent.spawn (RFC 0005/0009) briefly
   │    returns -32000 "reload in progress" to NEW spawns (mirrors the `draining`
   │    guard, but transient — cleared in step 6).
   ├─ let in-flight subagent turns reach a TURN BOUNDARY (RFC 0007 loop invariant).
   │    we do NOT cancel in-flight work; we wait up to AGENTD_RELOAD_QUIESCE
   │    (default 10s, MUST be < drain_timeout) for a natural quiesce point.
   └─ in reactive mode, quiesce = the idle point between routed events (RFC 0008).

4. APPLY the reloadable diff (idempotent, per-subsystem)
   ├─ MCP servers:   stop+reap removed servers via the stdio shutdown ladder
   │                 (close-stdin→SIGTERM→SIGKILL, RFC 0004/0003 §3.5);
   │                 spawn+handshake added servers (RFC 0004); leave unchanged ones.
   ├─ subscriptions: unsubscribe removed URIs; for ADDED URIs, subscribe AND
   │                 read-after-subscribe (MANDATORY — RFC 0003 §3.11) to convert
   │                 edge→level across the reload boundary; leave unchanged ones.
   ├─ model/headers: swap the resolved values; next intel call uses them.
   ├─ limits:        install as the template for NEW spawns/turns (§5.5).
   └─ log_level:     apply immediately.

5. SELF-MCP surface refresh
   ├─ emit notifications/tools/list_changed if the tool set changed (RFC 0005 §3.1).
   ├─ emit notifications/resources/updated for agentd://config/effective (§4.2).
   └─ (server schema agentd://config/schema is unchanged unless the binary changed.)

6. CLEAR `reloading`, EMIT SUCCESS
   └─ {"event":"config.reloaded","changed":["mcp_servers","subscribe"],"ts":…} (info),
        bump agentd_config_reload_total{result="applied"}.
```

Key invariants:

- **Validate-before-apply, always.** Steps 1–2 are pure-CPU and complete *before*
  any subsystem is touched. An invalid new config (or one touching a restart-only
  field) is a **clean no-op**: the running config is byte-for-byte unchanged.
- **All-or-nothing on the reloadable diff.** Step 4 is ordered so a partial OS
  failure (e.g. an added MCP server fails to handshake) is **contained**: a failed
  *add* is logged as a `mcp.connect.fail` (RFC 0010) and that one server is marked
  unavailable (the model sees a tool-domain absence, RFC 0007), but it does **not**
  roll back the servers already applied — because each sub-apply is independently
  valid (the *config* validated in step 1; only a *runtime* connect can fail, and
  that is the same degradation the daemon already tolerates at startup, RFC 0004).
  We never leave config in a state that did not validate.
- **In-flight work is preserved where possible** (step 3): we quiesce to a turn
  boundary rather than cancelling, so a reload mid-run does not discard partial
  results. If quiesce times out (`AGENTD_RELOAD_QUIESCE`), we still apply — the
  applied diff only affects *new* turns/spawns, so a slow in-flight turn finishes
  on the old MCP set and the next turn sees the new one (§5.5).

### 5.4 `reload_coherence_check()` — what §4.1 adds beyond RFC 0011 §3.3

```rust
// run by BOTH --validate-config and the reload path. pure-CPU.
fn reload_coherence_check(new: &Config, running: Option<&Config>) -> Result<(), Vec<Diag>> {
    let mut diags = Vec::new();
    // 1. restart-only fields should not be FILE-set (they belong in env/flag).
    for f in RESTART_ONLY_FILE_KEYS {
        if new.file_set(f) { diags.push(Diag::warn(f, "restart-only field set in file; \
            it will require a pod restart to change — prefer env/flag")); }
    }
    // 2. on a live reload, the diff must not touch a restart-only field.
    if let Some(run) = running {
        for f in RESTART_ONLY_FIELDS {
            if new.get(f) != run.get(f) {
                diags.push(Diag::reject(f, "restart-only field changed; reload refused, \
                    restart required")); // → step 2 ABORT
            }
        }
    }
    // 3. the reloadable subset is internally consistent (e.g. a subscription URI
    //    references a declared mcp_server; a server name is unique; tags are valid).
    check_subscriptions_reference_declared_servers(new, &mut diags);
    check_unique_server_names(new, &mut diags);
    if diags.iter().any(Diag::is_error) { Err(diags) } else { Ok(()) }
}
```

The **warn** vs **reject** distinction: a restart-only field merely *present in the
file* is a warning (it works, it just pins you to restart-to-change); a restart-only
field whose value *differs on a live reload* is a reject (the running process cannot
honour it). `--validate-config` reports both; the reload path acts on rejects.

### 5.5 Interaction with in-flight subagents and budgets

- **Limits are a spawn-time template.** A reloaded `limits` block changes the budget
  minted into **new** spawns and the bound checked on **new** turns. In-flight
  children keep the `grant_tokens`/`grant_steps`/`deadline` minted at their spawn
  (RFC 0003 §3.0/§3.8) — we never retroactively shrink a running child's budget (it
  could trip a kill mid-turn for no operator-visible reason). A reloaded *tighter*
  tree-token ceiling **does** apply immediately to the root counter (RFC 0003 §3.8),
  because that is a tree-global safety bound, not a per-child grant.
- **MCP server removal mid-run.** If a removed server is **in use** by an in-flight
  turn, removal waits for the quiesce boundary (step 3); the in-flight `tools/call`
  on that server completes, and the server is reaped only after. A turn that *starts*
  after the reload sees the server gone (a tool-domain absence, RFC 0007), never a
  mid-call disconnect.
- **Reactive subscription churn** is exactly RFC 0008's reconcile, run at a reload
  boundary instead of a restart boundary: added subs get read-after-subscribe so no
  level-state is missed; removed subs stop routing. This is why subscription reload
  is *safe* — it reuses the already-proven restart reconcile (RFC 0003 §3.11), not a
  new mechanism.

### 5.6 Observability of a reload (RFC 0010 owns the surface)

Reload is a first-class, closed-vocabulary event family (RFC 0010 §3.3 owns the
schema; this RFC names the members):

- `config.reload_requested` (info) — `{trigger:"sighup"|"watch"}`.
- `config.reloaded` (info) — `{changed:[…field groups…], applied_ms}` on success.
- `config.reload_rejected` (warn) — `{reason:"invalid"|"restart_required", field?, diagnostics[]}`.
- Metric: **`agentd_config_reload_total{result="applied"|"rejected"}`** (counter,
  RFC 0010 naming convention `agentd_*_total`), and a gauge
  **`agentd_config_generation`** incremented on each applied reload so a scraper
  can detect "this instance has picked up generation N" against the desired
  generation agentctl tracks. The reload **never** flips `/healthz` (liveness) or
  trips the stuck-detector; a rejected reload leaves a healthy, running process
  (RFC 0010 §health). On a successful reload `agentd://config/effective` fires
  `notifications/resources/updated` (§4.2) so a subscribed agentctl learns the new
  generation push-style.

---

## 6. Mechanisms — file-based secret refs (rotation)

Secrets remain env/file-only, never in the config file, never logged (RFC 0012
§3.7 is the authority; this RFC adds two *file-backed* sources to the same
`resolve()` front door, owned by RFC 0006 §6).

### 6.1 `--intelligence-token-file <path>`

A sibling to `--intelligence-token` / `AGENTD_INTELLIGENCE_TOKEN` (RFC 0011 §3.2)
that reads the intelligence credential from a **mounted file** rather than an env
var — the idiomatic shape for a Kubernetes `Secret` volume:

```
--intelligence-token-file /var/run/secrets/intel/token
AGENTD_INTELLIGENCE_TOKEN_FILE=/var/run/secrets/intel/token
```

- Resolved through `secrets::resolve()` (RFC 0006 §6) into the same `Secret`
  newtype (RFC 0012 §3.7 — `Debug`/`Display` = `***`, no `Serialize`).
- **Re-read on use, not cached at startup**, so a Kubernetes Secret rotation
  (kubelet atomically swaps the projected file) takes effect on the **next intel
  call** with no restart. A read failure at use-time is the existing
  `EXIT_INTELLIGENCE`-class auth/unreachable path (RFC 0011 §5, code `4`) for a
  one-shot, or a logged retry for a daemon — not a crash.
- Precedence among credential sources (env-alias source → live-read file → process
  env `name`) is **owned by RFC 0006 §6**; this RFC only adds the file path as a
  recognized source and the re-read-on-use property for rotation.

### 6.2 `{{secret-file:PATH}}` interpolation

The declared intelligence-header interpolation (RFC 0006 §3, RFC 0012 §3.7)
currently resolves `{{secret:NAME}}` (env/configured-source). This RFC adds a
sibling token **`{{secret-file:PATH}}`** that reads the named **mounted file** at
the moment the wire bytes are written:

```jsonc
"intelligence_headers": {
  "authorization": "Bearer {{secret-file:/var/run/secrets/intel/token}}",
  "x-api-key":     "{{secret:ANTHROPIC_API_KEY}}"
}
```

Rules (all inherited from RFC 0012 §3.5/§3.7, restated as the boundary):

- The **template** (`{{secret-file:/path}}`) is structural config and **may** live
  in the config file or a flag; the **resolved value** is materialized only at the
  instant of writing the request bytes, **after** CR/LF header validation (RFC 0012
  §3.5), is itself CR/LF-checked, and is **never** retained on the heap past the
  request, **never** logged, **never** in a transcript or checkpoint.
- The path is **read fresh on each use** (same rotation property as §6.1). A path
  that does not exist / is unreadable at resolution is a config error at startup
  (validated — exit `2`, RFC 0011) and a logged request error at reload/use time.
- `{{secret-file:…}}` is subject to the same `read_local` constraint as the config
  file (local filesystem path only) — no scheme, no network.
- A `{{secret-file:…}}` value appearing in any **non**-secret-bearing field (i.e.
  anywhere the field allowlist, RFC 0010, would log) is rejected at validation, so
  a secret ref cannot be smuggled into a logged field.

The config file thus holds only the **reference** (`{{secret:…}}` /
`{{secret-file:…}}`), never the secret — the RFC 0011/0012 invariant that the file
is secret-free holds exactly: a reference is structural, the value is not in the
file.

---

## 7. Failure semantics

The single rule, applied at every entry point: **resolve precedence, validate
fully, then either apply atomically or no-op cleanly — never half-apply.**

| Situation | Behaviour | Exit / event |
|---|---|---|
| **Bad config at startup** (file parse error, unknown key, invalid value, secret-in-file, drain≥grace) | fast-fail **before any side effect** (RFC 0011 §3.3) | exit `2`, one `config.invalid` line |
| **`--validate-config` on bad config** | collect **all** diagnostics, no side effect | exit `2`, N `config.invalid` lines |
| **`--validate-config` on good config** | no side effect | exit `0`, `config.valid` |
| **Bad config on reload** (file now invalid) | **no-op**: keep old config verbatim, do not touch any subsystem | stays running; `config.reload_rejected{reason:"invalid"}` (warn) + metric `result="rejected"` |
| **Reload touches a restart-only field** (§5.1) | **no-op**: keep old config; signal that a restart is required | stays running; `config.reload_rejected{reason:"restart_required",field}` (warn) |
| **Reload valid; a single added MCP server fails to handshake at apply** | apply the rest; mark that server unavailable (tool-domain absence to the model, RFC 0007) | `config.reloaded` (info) + `mcp.connect.fail` (warn) for the one server; not a rollback |
| **Reload while `DRAINING`** | ignored — drain wins (the process is exiting) | no event beyond the drain's own |
| **Quiesce times out** (`AGENTD_RELOAD_QUIESCE`) | apply anyway; the diff affects only new turns/spawns (§5.5) | `config.reloaded` with `quiesce_timeout:true` |
| **Secret file missing/unreadable** at startup | config error before side effect | exit `2`, `config.invalid` |
| **Secret file missing/unreadable** at use/reload | per-request error; one-shot → `EXIT_INTELLIGENCE` (4); daemon → logged retry | (RFC 0011 §5) |
| **Precedence conflict** (env/flag override a file value, or `--mcp`/`--subscribe` add to the file list) | resolve by RFC 0011 §3.1 (flag>env>file>default); list-add for repeatable flags (§3.2) | **not an error** — `config.loaded` records the resolved set; provenance per key is visible in `agentd://config/effective` |

Two non-negotiables, restated:

1. **Validate-before-apply is universal.** Startup, `--validate-config`, and reload
   all run the *same* `Config::validate()` + `reload_coherence_check()` before any
   mutation. There is exactly one validation pipeline, so the admission gate, the
   startup path, and the reload path can never disagree.
2. **A failed reload is observably a no-op.** The running process is byte-for-byte
   unchanged, stays healthy, and emits one `config.reload_rejected` event. agentctl
   treats that as "desired config not yet effective" and either fixes the config
   (for `invalid`) or rolls a restart (for `restart_required`) — its policy, on
   agentd's primitive.

---

## 8. Minimalism & the agentd/agentctl boundary

Holding the moat (RFC 0014 §3 principle 3), per surface:

| Surface | Dependency cost | Feature gate |
|---|---|---|
| Config file (JSON) | **zero** — `serde_json`, already linked | default build |
| Config file (YAML) | one small YAML reader | `config-yaml`, **off** by default & in cloud-native images |
| `--validate-config` | zero — same loader/validator | default build |
| `--config-schema` | zero — in-tree `schema::Describe` derive, **no `schemars`** | default build |
| Hot reload via `SIGHUP` | zero — raw `sigaction` + self-pipe (RFC 0003 §3.1) | `hot-reload` |
| Hot reload via file-watch | zero crates — raw `inotify` (`libc`) | `config-watch`, **off** by default |
| `--intelligence-token-file`, `{{secret-file:…}}` | zero — file read + existing `resolve()` (RFC 0006 §6) | default build |

**Nothing here pulls an async runtime, a Kubernetes client, or a TLS/gRPC stack.**
The reload path runs on the existing single-threaded `recv_timeout` reactor (RFC
0002/0003), driven by the same self-pipe-on-signal mechanism the rest of the
supervisor uses. There is no watcher thread pool, no inotify *crate*, no schema
*crate*.

**What is explicitly agentctl's, not agentd's** (RFC 0014 §6): the CRD
(`Agent`/`AgentFleet`) and its config sub-object; the **admission webhook** that
*calls* `--validate-config`/the schema (agentd ships the verdict, not the webhook);
the operator **reconcile loop** that decides *when* to push a new `ConfigMap` and
whether to SIGHUP vs roll a restart on a `restart_required` reject; the **rollout
strategy** (surge/maxUnavailable) for restart-only changes; the
`ConfigMap`-to-volume projection. agentd exposes the file, the validator, the
schema, the signal, and the reload — never the Kubernetes-facing policy that
composes them.

---

## 9. Non-goals / Deferred

- **No remote/network config.** Unchanged from RFC 0011 §3.1 — all input is env +
  flags + a **local** file (and now local secret-volume files). `--config` is
  `read_local`; `{{secret-file:…}}` is `read_local`. No URL scheme, ever.
- **No reload of mode, transport, identity, exec, or serve-mcp.** Those are
  **restart-only** (§5.1); a reload touching them is a clean reject, and agentctl
  rolls a restart. This RFC deliberately does *not* grow live mode-switching or
  live socket-rebinding — they reintroduce exactly the "partial re-handshake /
  identity churn" class RFC 0011 §2.2 warned about.
- **Intelligence endpoint hot-swap / multi-endpoint failover is NOT here.** Live
  changing of the intel transport/endpoint is **RFC 0018** (intelligence transport
  resilience). This RFC keeps the intel transport restart-only and reloads only the
  *params* (`model`, `max_tokens`, declared headers) that are safe to swap.
- **No YAML in the default or cloud-native build.** `config-yaml` is off; JSON is
  the dependency-free default (§3.2, §8).
- **No `schemars`/`jsonschema` crate.** The schema is generated by an in-tree derive
  from the same types (§4.2); a schema *library* is binary weight the moat forbids.
- **No config templating / env-substitution language in the file** beyond the two
  secret-ref tokens (§6). The file is data, not a template engine; environment
  variation is env/flag's job (12-factor III).
- **No durable config history / rollback store.** agentd holds the *current*
  resolved config (and the prior one only long enough to diff a reload); versioning,
  history, and rollback are agentctl/GitOps concerns (the `ConfigMap` is the source
  of truth, not an agentd-side store). This is consistent with the stateless-
  supervisor stance (RFC 0011 §7, RFC 0003 §3.11).
- **No checkpoint of dynamic (self-MCP `subscribe`) subscriptions across reload.**
  As with restart (RFC 0011 §7, RFC 0013 D8), only **declared** subscriptions are
  reconciled on reload; dynamic ones are recovered by idempotent re-trigger.

---

## 10. References

- **RFC 0002** — supervisor reactor & concurrency: the `recv_timeout` reactor +
  self-pipe the reload routine runs on; no new thread/runtime is introduced.
- **RFC 0003** — process supervision & recovery: the quiesce/kill ladder (§3.5) the
  reload reuses, the stdio MCP-server shutdown ladder, and **rebuild + reconcile /
  read-after-subscribe** (§3.11) that the subscription reload reuses verbatim.
- **RFC 0004** — MCP client subset & codec: connecting/handshaking added MCP servers
  and the stdio-child shutdown for removed ones on reload; the wire is owned there.
- **RFC 0005** — self-MCP server & control protocol: the `agentd://config/schema`
  and `agentd://config/effective` resources, `notifications/tools/list_changed` on a
  changed tool set, the `reloading`/`draining` spawn guard; the surface is owned there.
- **RFC 0006** — intelligence transport & wire: owns `secrets::resolve()` and the
  `{{secret:NAME}}` declared-header interpolation this RFC extends with
  `{{secret-file:…}}` and `--intelligence-token-file`.
- **RFC 0007** — agentic loop & terminal status: the per-turn boundary the reload
  quiesces to; a removed-server tool-domain absence is an observation, not a crash.
- **RFC 0008** — execution modes & reactive routing: owns the subscription
  reconcile and the exit predicate that makes `mode` restart-only.
- **RFC 0009** — subagent process model: spawn chokepoint the `reloading` guard
  gates; reloaded `limits` are a spawn-time template, not a retroactive shrink.
- **RFC 0010** — observability, health & telemetry: **owns** the event vocabulary
  (`config.reloaded`/`config.reload_rejected`/`config.loaded`) and metric naming
  (`agentd_config_reload_total`, `agentd_config_generation`) this RFC's events slot
  into; reload never flips `/healthz`.
- **RFC 0011** — cloud-native contract: **the RFC this extends** — owns config
  precedence (§3.1), validate-at-startup → exit 2 (§3.3), the signal table (§4.1,
  which this RFC amends only for SIGHUP), the drain choreography (§4.2), the
  exit-code table (§5), and the secrets-not-in-the-file rule (§3.2).
- **RFC 0012** — security posture: owns secrets handling (§3.7 — env/file only,
  `Secret` newtype, never logged) and the CR/LF header validation
  `{{secret-file:…}}` resolution rides; the file stays secret-free.
- **RFC 0013** — deferred v2 surface: recorded the declarative-config surface as a
  defer line (this RFC lands it) and owns the dynamic-subscription /
  warm-session-checkpoint deferral the reload path inherits.
- **RFC 0014** — control-plane contract (the umbrella): the primitives-not-policy
  split, the frozen `contract_version` the schema carries, and the agentctl-owned
  admission/reconcile/rollout policy that consumes these primitives.
- **RFC 0018** — intelligence transport resilience (sibling): owns live endpoint
  failover / model-endpoint hot-swap, which is why this RFC keeps the intel
  *transport* restart-only and reloads only intel *params*.
```
