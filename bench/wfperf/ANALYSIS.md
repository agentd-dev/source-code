# Workflow execution performance — analysis and results

*2026-08-23. Method: `bench/wfperf/run.sh` (five representative shapes through
the real reactor, release profile, per store kind) + `perf` cycle profiles of
a 400-step chain on the file store (symbols via a `strip=false` build).*

## Baseline profile (where the time actually went)

A 400-assign chain on the file store spent, of total cycles:

| Cost centre | Share | Cause |
|---|---|---|
| `checkpoint` → `Durable::put` → `FileStore::put` | **~40%** | every step start AND finish serialized the whole `RunState` and wrote it; `FileStore::put`'s seq-CAS **read and parsed the previous envelope back from disk on every write** (`FileStore::read` 10%, `serde_json::from_slice` 10%) |
| `Workflow::clone` in `definition_for_run` | **~10%** | the full definition graph (a `BTreeMap<String, Step>`) deep-cloned per step, several times per step |
| `run_data` | **~10%** | per-step rebuild of the template data view: full deep clone of `inputs` + every step's output + `vars`, plus a whole-definition walk for `memory.<key>` references |
| allocator (`_int_malloc`/`_int_free`/consolidate) | ~30% self | the churn behind all of the above |
| `__memcmp_avx2_movbe` | ~9% | `BTreeMap<String, …>` key comparisons |

CEL additionally re-parsed (ANTLR, under `catch_unwind`) every expression on
every evaluation.

## Implemented (this commit)

1. **FileStore seq cache.** The instance `flock` makes the process the only
   writer, so the last-written seq per key is exact in memory; the per-put
   CAS now compares against the cache and only reads disk the FIRST time a
   key is touched (a previous life's state is respected as before).
2. **`Arc<Workflow>` definitions.** `workflows`/`pinned` hold
   `Arc<Workflow>`; `definition_for_run` is a refcount bump. The one
   mutation (arming) is `Arc::make_mut` copy-on-write.
3. **Checkpoint-before-effect only before effects.** Pure data kinds
   (`assign`/`map`/`filter`/`reduce`/`sort`/`dedupe`/`chunk`/`parse`/
   `switch`/`noop`/`assert`) have no external effect: a crash replays them
   deterministically from the last checkpoint (RFC 0025 §7 guards effects,
   and these have none). Their per-step `checkpoint()` calls are skipped —
   an inline chain now batches into its tick's single checkpoint. A
   completion that makes the RUN terminal still checkpoints immediately, and
   `kind: checkpoint` still forces one.
4. **CEL program memoization** per expression text (thread-local, the
   reactor is single-threaded; capped at 4096 programs).
5. **Memoized `memory.<key>` scan** per definition content hash (was a
   whole-definition recursive walk per step).
6. **Same-iteration stream wake.** An `emit` sets a dirty flag; the loop
   re-runs `poll_stream_starts` + scheduling in the SAME iteration (bounded,
   like the inline fixpoint), so a same-process produce→consume pipeline no
   longer waits out the tick park in the worst case. (The bench shape is
   `idle_grace`-dominated either way; the win is the quiet-reactor case.)
7. **A completed foreach batch checkpoints explicitly.** Body steps are
   commonly pure and no longer checkpoint per element, so the "a restart
   resumes at the next batch" durability point is written at the batch
   boundary itself — one write per batch instead of one per element.

## Measured results (same machine, same shapes, wall ms)

| shape | memory: before → after | file: before → after |
|---|---|---|
| chain (200 chained assigns) | 729 → **146** (5.0×) | 1564 → **155** (10.1×) |
| fanout (300-item foreach) | 1071 → **485** (2.2×) | 2475 → **612** (4.0×) |
| interp (interpolation-heavy) | 95 → **22** (4.3×) | 383 → **32** (12.0×) |
| cel (100 CEL-gated steps) | 275 → **116** (2.4×) | 670 → **124** (5.4×) |
| events (40 emits → 40 runs) | 459 → 454 (~1×) | 670 → 637 (~1×) |

(Wall includes a fixed ~400 ms `idle_grace`; the events shape is bounded by
stream-poll batching across ticks, not CPU — see "Not taken".)

## Known-remaining opportunities (ranked, not yet taken)

- **`run_data`'s steps view is still O(run size) per step.** The clean fix
  is an incrementally-maintained cached `steps` `Value` on `RunState`
  (patched in `end_step`), or a borrow-based template resolver. Deep-clone
  cost remains for runs with very large step outputs.
- **`opt-level = "z"` costs a measured ~20–30%.** Rebuilt at `opt-level = 3`
  (everything else equal): chain 146→112 ms, cel 116→84 ms, interp 22→18 ms,
  fanout 485→398 ms — for a binary of 10.8 MB instead of 7.3 MB (+47%).
  Whether that size is worth the speed is a distribution decision
  (containers, install.sh, embedded/PID-1 targets), so it is REPORTED here,
  not changed.
- **Events tick latency.** `poll_stream_starts` handles ≤32 events per arm
  per pass, once per wake; a hot stream pays tick-cadence latency. An
  explicit wake when `emit` appends (like the inline fixpoint) would make
  same-process produce→consume near-synchronous.
- **`BTreeMap<String, Value>` memcmp.** serde_json's map is BTree; the 9%
  memcmp would shrink with `preserve_order` (IndexMap) — a workspace-wide
  semantic change (key ordering), not worth it blind.
- **Turn/intelligence path** was not profiled here: it is dominated by
  network latency, not engine cycles.

## Reproducing

```console
$ bash bench/wfperf/run.sh target/release/agentd memory <label>
$ bash bench/wfperf/run.sh target/release/agentd file <label>
# profile (needs a symbolized build):
$ CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=true \
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --features workflow,cel
$ perf record -F 999 --call-graph fp -- target/release/agentd --config <bench yaml>
```
