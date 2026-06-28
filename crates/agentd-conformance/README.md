# agentd-conformance

A **black-box** conformance suite for the agent runtime. It knows agentd only as
a binary plus the MCP / JSON-RPC 2.0 spec and the documented exit-code table — it
never links the agentd library, so it catches real protocol/behaviour
regressions instead of agreeing with the implementation's own types.

Each check drives the real binary (built on demand with `--features serve-mcp`)
and asserts one contract. The same checks back both the `cargo test` integration
tests and the `agentd-conformance` report runner.

## Running

```sh
cargo test -p agentd-conformance          # every check as a #[test], one test per family
cargo run  -p agentd-conformance          # the same checks → a PASS/FAIL report
cargo run  -p agentd-conformance -- --json   # machine-readable conformance record
```

The suite builds the agentd binary itself, so no prior `cargo build` is needed.
Every check is host-independent — conformance is judged against the spec, never
the environment — so there are no capability-gated checks to skip.

## Families

| Family        | What it proves                                                          |
|---------------|-------------------------------------------------------------------------|
| `mcp-server`  | agentd's served self-MCP: initialize, tools/list, tools/call, resources, ping, error codes (-32601 / -32602 / -32002), notification + malformed-input handling. |
| `mcp-client`  | agentd as a client to a backing server (a recording `confmcp` reference server): initialize w/ protocolVersion + clientInfo, `notifications/initialized`, tools/list discovery, resource subscribe. |
| `supervisor`  | the exit-code table (0 / 2 / 4 / 6) and the SIGTERM graceful-drain → exit 0. |
| `agent-loop`  | the ReAct loop end-to-end: a direct answer, a tool call executed + fed back, multi-step convergence, and `--max-steps` bounding a non-converging loop. |
| `security`    | the Rule-of-Two lethal-trifecta refusal + its `--allow-trifecta` override, and that the intelligence token never leaks into telemetry. |

## Adding a check

Append a `Check { id, category, desc, run }` to the relevant family's
`checks()`. The `run` function takes `&Harness` and returns an `Outcome`
(`pass` / `note` / `fail` / `require(cond, why)`). It is picked up automatically
by both the tests and the runner.
