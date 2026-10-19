# agentd-conformance

A **black-box** conformance suite for the agent runtime. It knows agentd only as
a binary plus the specifications it serves — A2A / JSON-RPC 2.0, the OAuth RFCs,
the Personal Agent Protocol draft — and the documented exit-code table. It
never links the agentd library, so it catches real protocol/behaviour
regressions instead of agreeing with the implementation's own types; it uses
`agentd-net` for HTTPS and `ring` for P-256 signatures, the transport and the
cryptography, and writes every protocol message itself.

Each check drives the real binary (built on demand with
`--features a2a,cron,internal-mocks,oauth,companies`) and asserts one contract. The same checks
back both the `cargo test` integration tests and the `agentd-conformance` report
runner.

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

| Family             | What it proves                                                     |
|--------------------|--------------------------------------------------------------------|
| `supervisor`       | the exit-code table: a completed `once` job exits 0, an unknown flag 2, an unreachable intelligence endpoint 4. |
| `security`         | the lethal-trifecta refusal at startup, and that the intelligence token never leaks into telemetry. |
| `store`            | a job backed by an MCP store runs, and a restarted instance restores the completed run without re-firing its `once` start. |
| `durability`       | a SIGKILL before and during a step is recovered by the next life.   |
| `tools`            | internal tools round-trip to the supervisor, an invented tool is answered as an error, and `--capabilities` lists the registry. |
| `a2a-conversation` | the core A2A 1.0 surface: the version gate and the method vocabulary, reads answered as Messages with no task, turns as task artifacts with their history, `input-required` on the core wire, push delivery, the card's promises, and the error codes. |
| `events`           | the observation feed the events extension declares: off by default, strict params, `hello` + ring replay. |
| `extensions`       | activation by `A2A-Extensions` is the only way in (commands and methods), results are a Task or a Message, and metadata keys are declared extension URIs. |
| `auth`             | the card's security fields match what the listener enforces, the extended card needs a declared credential, and browsers are admitted only from listed origins. |
| `mcp-auth-core`    | agentd as an OAuth client: RFC 8414 and RFC 9728 metadata read first at the path-inserted well-known URLs, a mismatched issuer refused, and the refresh token revoked at the server on `--logout`. |
| `mcp-tasks`        | agentd as a client of the MCP Tasks extension (SEP-2663): a workflow step's task followed to its result, its timeout's `tasks/cancel`, and `tasks: off` against a server that requires the extension. |
| `mcp-skills`       | agentd as a client of the MCP Skills extension (SEP-2640): a listed skill loaded once its `SKILL.md` matches its entry's digest and size, one whose bytes differ declined after a `skills/get` refresh, and a server's prompt that is never a skill. |
| `companies`        | the fake Company agentd is tested against under the Personal Agent Protocol draft pinned at `95fcbf2`, driven by a Personal Agent written here from the draft: each check quotes the sentence it tests, passes against the fake, and catches the variant built to break it; then agentd itself as the Personal Agent, read from the fake's record of what reached it. |
| `ema`              | agentd acting for one enrolled person at MCP servers through their IdP (RFC 0044): the ID-JAG exchange and redemption on the wire, every check an ID-JAG is held to, renewal, re-presentation, step-up, revocation and the freshness kill-switch, re-enrolment, the pins the server's RFC 9728 metadata confirms, the person's consent gates (a workflow step's own egress too), work a subagent or a schedule started, and the document boundary — against a mock IdP, authorization server and EMA MCP server (`agentd --internal-mock-xaa`). |

Every check, and what it proves, is listed in the repository's
[`CONFORMANCE.md`](../../CONFORMANCE.md).

## Adding a check

Append a `Check { id, category, desc, run }` to the relevant family's
`checks()`. The `run` function takes `&Harness` and returns an `Outcome`
(`pass` / `note` / `fail` / `require(cond, why)`). It is picked up automatically
by both the tests and the runner; add its row to the family's table in
`CONFORMANCE.md`, which a unit test holds to the registration. A new family is
a `Category` variant listed in `Category::ALL`, an arm of `family()`, a
`#[test]` in `tests/conformance.rs` and a row above: the compiler requires the
arm, and the suite's tests require the `#[test]` and both tables' rows for
every family in `Category::ALL`.
