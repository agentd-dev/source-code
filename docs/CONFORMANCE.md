# Conformance, reliability, and cost

`crates/agentd-conformance` is the executable form of the runtime's
promises. A corpus that passes is a runtime that still does what the
RFCs say — under repetition, under fault, and under attack — and a
record of what it costs to do so.

It drives the **real** engine through agentd's public API: the same
control handlers, tool families, intelligence handler, and policy
enforcement the daemon uses. Only the intelligence backend is a mock
(seeded canned responses), so every scenario is deterministic given its
trial index — no network, no flakiness, fully replayable.

```
cargo run -p agentd-conformance -- crates/agentd-conformance/corpus
cargo run -p agentd-conformance -- crates/agentd-conformance/corpus --json
cargo test  -p agentd-conformance       # the corpus is a CI gate
cargo bench -p agentd-conformance       # engine throughput + cold-start
```

## Scenarios

A scenario is one `*.toml` file: a workflow (inline or by path), a
trigger, the canned `llm_infer` responses its nodes should see, an
optional policy to enforce, and the expected outcome / trace / cost.

```toml
name = "llm-route"
capabilities = ["llm_infer", "switch", "terminate", "trigger_manual"]
trials = 1

[workflow]
inline = """
name = "llm_router"
[[start_nodes]]
name = "main"
source = "manual"
entry_node = "classify"
[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"
prompt = "Classify the document."
input_from = "trigger"
output_schema = "inline"
[[nodes]]
id = "route"
type = "switch"
expr = "classify.parsed.decision"
[[nodes]]
id = "done_alpha"
type = "terminate"
[[edges]]
from = "classify"
to = "route"
[[edges]]
from = "route"
when = "alpha"
to = "done_alpha"
"""

[[intel.turns]]
content = '{"decision": "alpha"}'
prompt_tokens = 120
completion_tokens = 8

[expected]
status = "completed"        # completed | failed | timed_out | errored
last_node = "done_alpha"
path = ["classify", "route", "done_alpha"]
path_exact = true
max_llm_calls = 1
max_total_tokens = 200
```

Intelligence responses are ordered **turns** — one per successive
`llm_infer` call — each offering one or more **variants**. A reliability
run seeds a different variant selection per trial.

## The metrics

### pass^k (reliability)

Borrowed from tau-bench: run a scenario `trials` times and it passes
only if **every** trial passes (`pass^k`). With multiple response
variants per turn, the trials sample model nondeterminism. A bounded
workflow that routes every possible answer holds `pass^8 = 1.0` where a
fragile one decays — a measurable differentiator, not a slogan. The
suite reports per-scenario `pass^k` and the mean across the corpus.

### Reliability-gated autonomy

Autonomy is earned, measured. A scenario reports a continuous
`pass_rate` (fraction of trials passed) alongside the strict `pass^k`,
and can declare the bar it must clear to be trusted:

```toml
trials = 8
min_pass_rate = 0.95   # this workflow must pass ≥ 95% of trials
```

A scenario that declares a `min_pass_rate` passes when it clears that
bar (tolerated flakiness); without one, the strict "every trial" rule
applies. The CLI adds a suite-wide floor:

```bash
agentd-conformance run corpus/ --min-pass-rate 0.95   # CI deploy gate
```

Any scenario below the higher of its own bar and the floor fails the
run (exit non-zero) — independent of pass/fail tallies. This is the gate
that decides whether a workflow has earned the right to run unattended:
certify it in CI, promote it (`agentd --promote`), then deploy it. The
strict `pass^k` is always reported, so the headline number stays honest
even when a tolerated bar is set.

### Capability coverage (goal tracking)

Every scenario tags the capabilities it exercises against a canonical
[capability matrix](../crates/agentd-conformance/src/capability.rs).
Coverage is the fraction of the matrix touched by at least one
**passing** scenario; the uncovered set is the suite's visible backlog.
Tags outside the matrix are flagged so a typo can't inflate the number.

### Fault tolerance (robustness battery)

Faults must degrade predictably — a bounded stop, never a hang or a
runaway. The battery injects:

- **malformed output** — `output_schema` rejects non-JSON: a bounded
  `errored` stop with cost still accounted;
- **backend down** — the request itself fails: zero tokens billed, no
  hang;
- **schema drift** — a valid-but-unrouted answer dead-ends at the
  switch, completing bounded at the unroutable node.

### Security conformance (the lethal-trifecta cut)

Prompt injection can poison an `llm_infer` output, but the model fills
exactly one node — it cannot pick tools or edges. When the poisoned
output redirects a downstream side-effect outside the policy allowlist
(a file write to an escaping path, an HTTP call to an exfil URL), the
static policy denies it before the action and records the denial. A
security test drives the write injection against a real temp directory
and asserts the escaping file never reached disk: the denial *prevents*
the side-effect, it does not merely count it.

### Cost-per-success

Raw token cost rewards corner-cutting; cost-per-success rewards getting
the job done reliably. The suite sums cost across every trial and
divides by the trials that passed, so a workflow that retries its way to
green pays for it. Reported per scenario and across the suite.

### Cost forecasting & drift detection

Two products fall out of a deterministic substrate plus a cost/reliability
harness:

```bash
# Project spend at a trigger rate (cost-per-success is a measured
# constant, so spend scales linearly with volume).
agentd-conformance run corpus/ --forecast-runs-per-day 5000 --price-per-mtok 5
#   → forecast @ 5000 runs/day: 99 tokens/success → … tokens/month (~$74/month)

# Save a baseline, then gate future runs against it.
agentd-conformance run corpus/ --save-baseline baseline.json
agentd-conformance run corpus/ --baseline baseline.json
#   → drift vs baseline — REGRESSIONS:  classify  pass_rate 1.00 → 0.70
```

Drift detection compares each scenario's `pass_rate` against the
baseline and fails on a regression — the "a model update silently broke
my workflow" alarm. Run it on a schedule against the live model and a
reliability drop pages you before users notice.

## Benchmarks

`cargo bench` quantifies the appliance claim — a single native binary
with no runtime or interpreter on the hot path:

- **engine throughput** — steady-state cost of walking a 20-node graph
  on a pre-built engine;
- **cold-start to first node** — build the handler registry + engine
  and execute the first node from scratch, the start-up latency a
  one-shot `--mode once` invocation pays.

## Adding a scenario

Drop a `*.toml` under `crates/agentd-conformance/corpus/` (in
`conformance/`, `faults/`, or `security/`), tag its capabilities, and
declare its expectations. The corpus test runs the whole tree as a CI
gate; a fault or security scenario "passes" when the runtime degrades or
denies exactly as declared.
