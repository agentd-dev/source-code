# RFC 0005: Hot reload — snapshot semantics, fail-forward

**Status:** Accepted, implemented.
**Author:** Andrii Tsok
**Depends on:** RFC 0001 §4.3, RFC 0004.

## 1. Problem

Long-running serve-mode processes accumulate operational pressure:
TLS certificates expire, webhook secrets and JWKS rotate, policies
tighten, MCP children wedge. Restarting drops in-flight requests and
resets rate-limit state; *not* restarting means running with stale
credentials. We need in-place reload for the rotating parts without
turning the runtime into a dynamic-everything system.

## 2. Decision

**Trigger.** `SIGHUP` on Unix; an mtime bump on `--reload-file`
everywhere (Windows has no console signal — a touched file is the
portable channel, and it composes with k8s ConfigMap projections).

**Scope — what reloads:** TLS cert/key (+ client-auth CA), prepared
auth (bearer/HMAC bindings, OIDC JWKS re-parse), the policy manifest
(including Rego recompile), per-server MCP allowlists and child
respawn, the intel client (bearer re-read), and the HTTP route +
rate-limit-bucket table.

**Scope — what deliberately does not:** the workflow graph itself,
the handler registry, bind address, feature set. Structural change is
a deploy, with validation and signing in the path. Reload rotates
*parameters of the declared structure*, never the structure.

**Consistency: per-accept snapshots.** Each reloadable component
lives behind an atomic-swap cell (`ArcSwap`). A connection captures
its snapshot at accept; a reload mid-request completes against the
old state, new connections see the new state. No locks on the hot
path, no torn reads (routes and their buckets swap as one unit —
observing new routes with old buckets would be a correctness bug,
so they share a cell).

**Fail-forward.** Every stage of a reload validates before it swaps.
A bad Rego program, an unreadable cert, a JWKS that doesn't parse —
each logs `reload.failed` with its stage and *keeps the old state
live*. SIGHUP is "try this", not "commit this". The single per-server
exception: an MCP child that respawns fine gets its new allowlist
even if a sibling fails — servers are independent trust domains by
RFC 0004, and holding one server's rotation hostage to another's
spawn failure helps nobody.

**Rate-limit counters reset on swap**, by design: an operator
tightening limits mid-flood should not find the flood grandfathered
in.

## 3. Alternatives considered

- **Full config reload (re-read graph, rebuild engine).** Rejected:
  it reintroduces every startup failure mode mid-flight and quietly
  bypasses the signing story ("edit the file, HUP it" must not be a
  deploy path for *structure*).
- **Watch the config file and auto-reload.** Rejected as the default:
  half-written files and editor swap artifacts make mtime an unsafe
  commit signal for *credentials*; an explicit signal keeps the
  operator in the loop. (`--reload-file` is opt-in and watches a
  *separate* touch-file, not the config.)
- **A drain-and-exec self-restart.** Simple but loses keep-alive
  connections and process-lifetime metrics; still available to
  operators via systemd as a fallback.

## 4. Consequences

Reload state plumbing (`ReloadHandles` on the engine, swap cells in
the HTTP server) is the price; it bought us credential rotation with
zero dropped requests, and the audit stream narrates every reload
stage (`reload.started` … `reload.succeeded` / `reload.failed`).
