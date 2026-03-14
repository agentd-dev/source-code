# RFC 0001: `agentd`
## A bounded, workflow-driven micro-agent runtime with compile-time constrained capabilities

**Status:** Accepted — implementation in progress.
**Authors:** Andrii Tsok
**Intended audience:** Runtime engineers, platform engineers, security reviewers, SDK/infra implementers
**Implementation language:** Rust
**Tracked implementation:** `crates/agentd/`.

---

## 1. Summary

`agentd` is a small Rust runtime for executing **bounded intelligence workflows**. It is not a general autonomous agent. Instead, it executes predeclared workflows triggered by:

- external events or notifications
- inbound HTTP requests
- explicit invocation of a workflow from a specific start node

Each `agentd` binary exposes only a **limited, precompiled capability set**. The runtime may use an LLM or other intelligence backend for specific bounded reasoning steps, but it cannot invent new tools, new workflow paths, or new authority at runtime.

A workflow in `agentd` is defined as a **directed acyclic graph (DAG)**. This provides enough flexibility to express both simple and sophisticated workflow topologies while preserving predictability, analyzability, and termination guarantees.

`agentd` is designed for predictable, auditable, policy-driven automation where a small amount of intelligence is useful, but general agent autonomy is undesirable.

---

## 2. Motivation

Most current AI agent runtimes optimize for flexibility:

- dynamic tool discovery
- autonomous planning
- open-ended tool usage
- prompt-defined control flow
- runtime behavioral drift

That makes them useful for experimentation, but weak for:

- strict operational predictability
- hard security boundaries
- auditability
- deterministic deployment behavior
- enterprise and infrastructure automation

`agentd` exists to occupy a different design space:

- **bounded**
- **workflow-driven**
- **compile-time capability constrained**
- **event-oriented**
- **observable**
- **MCP-native**
- **small enough to reason about**

The goal is to make intelligence usable in narrow controllers and automations without turning the runtime into an unconstrained general agent.

---

## 3. Goals

### 3.1 Primary goals

`agentd` must:

1. Execute workflows triggered by:
   - events/notifications
   - HTTP requests
   - explicit workflow invocation from a named start node

2. Restrict behavior to:
   - compiled-in capabilities
   - configured workflows
   - permitted MCP resources/tools
   - enforced runtime policy

3. Treat intelligence as a bounded step inside a workflow, not as the owner of control flow.

4. Support first-class MCP integration:
   - connect to multiple MCP servers
   - subscribe to configured resources
   - read configured resources
   - call configured MCP tools

5. Model workflows as **DAGs** so that complex branches, merges, and multiple start paths are possible without introducing runtime cycles.

6. Provide strong operational introspection:
   - structured logs
   - traces
   - metrics
   - audit trail
   - health/readiness reporting

7. Be deployable as a single Rust binary, optionally with embedded configuration and embedded workflow definitions.

### 3.2 Secondary goals

`agentd` should:

- be easy to test using fixtures and replay
- support dry-run mode
- support sealed or baked builds
- support local/private intelligence backends over Unix socket or HTTP
- support schema-validated structured outputs from intelligence steps
- support build-time capability pruning through Rust build configuration and compile-time feature selection

---

## 4. Non-goals

`agentd` is not intended to be:

- a general-purpose autonomous agent framework
- a dynamic plugin host for arbitrary third-party runtime code
- a shell automation replacement with unrestricted OS access
- a long-horizon planner with open-ended self-directed behavior
- a conversational assistant runtime
- a full BPM engine
- a generic distributed orchestration system

In particular, `agentd` does **not** aim to support unrestricted runtime planning, arbitrary loop construction, dynamic tool discovery as core behavior, or arbitrary shell/command execution.

---

## 5. Design principles

### 5.1 Bounded intelligence
Intelligence is used for classification, extraction, routing, transformation, or constrained decision-making. It is not allowed to expand authority.

### 5.2 Policy over prompting
Prompts may influence reasoning, but prompts do not define security or authorization boundaries.

### 5.3 Compile-time authority
The binary should only include the tool families, transports, and optional integrations it is meant to use.

### 5.4 Declarative DAG workflows
Control flow is defined explicitly as a directed acyclic graph, not hidden in prompts.

### 5.5 Structured outputs
Intelligence steps must produce schema-validated structured data wherever possible.

### 5.6 Event-oriented execution
The primary runtime model is trigger → context → reasoning → guard → action.

### 5.7 Auditability first
Every meaningful decision and side effect should be observable.

### 5.8 No invisible loops
Workflow repetition must not be encoded as cyclic graph traversal. Repetition, retries, or recurring behavior must be modeled explicitly through bounded retry policies or through new external triggers that start fresh executions.

---

## 6. Core concepts

### 6.1 Agent
An `agentd` is a runtime instance that executes one or more declared workflows using a bounded set of capabilities.

### 6.2 Capability
A capability is a compiled-in authority such as:

- read file
- write file
- create directory
- delete file
- make HTTP request
- compute diff
- connect to MCP
- read environment variables

A workflow cannot use a capability that the binary does not contain.

### 6.3 Workflow
A workflow is a declared DAG of typed execution nodes with one or more valid entry points.

### 6.4 Trigger
A trigger is the external or internal event that starts a workflow.

Supported trigger categories:

- event/notification trigger
- HTTP trigger
- explicit start-node invocation trigger

### 6.5 Start node
A start node is a named entry point into a workflow DAG. A workflow may have more than one start node.

### 6.6 Intelligence step
A bounded reasoning step that sends input context to an intelligence endpoint and receives structured output.

### 6.7 Action node
A node that performs a side effect using a compiled-in capability or permitted MCP action.

### 6.8 Guard
A runtime condition evaluated before allowing a transition or side effect.

### 6.9 Execution
A single run of a workflow from a trigger or start node to completion, failure, or timeout.

---

## 7. Trigger model

`agentd` must support three first-class workflow initiation modes.

### 7.1 Event or notification trigger

A workflow may start when:

- an MCP resource update notification is received
- an internal system event is emitted
- a subscribed notification source delivers a matching event

Examples:

- `mcp.resource.updated`
- `mcp.resource.created`
- `webhook.received`
- `timer.fired`, if timers are added later

This is the default and most important operating mode.

### 7.2 HTTP request trigger

A workflow may start when `agentd` receives an inbound HTTP request matching configured criteria.

Typical uses:

- small internal automation endpoints
- controlled integrations
- request-to-decision workflows
- human or system initiated bounded actions

The HTTP endpoint must map to a specific workflow and start node. It must not provide arbitrary runtime freedom.

### 7.3 Explicit start-node invocation

A workflow may be started programmatically from a named start node.

This supports:

- testing
- replay
- debugging
- manual operations
- controlled direct execution from code or API

This should allow operators or integrating systems to invoke a workflow at a precise point without requiring the original external trigger.

---

## 8. Execution model

Each execution follows a bounded lifecycle.

### 8.1 Lifecycle

1. Trigger received
2. Workflow selected
3. Start node resolved
4. Initial context materialized
5. Workflow DAG executed
6. Guards evaluated before side effects
7. Result recorded
8. Execution terminated

### 8.2 Execution properties

Each execution has:

- unique execution ID
- workflow ID
- start node ID
- trigger metadata
- timeout budget
- trace context
- audit context
- execution-local state

### 8.3 Execution invariants

- execution must only traverse declared DAG edges
- the graph must remain acyclic
- node execution must be type-valid
- side effects must pass policy checks
- invalid intelligence output must fail closed unless explicitly configured otherwise
- no node may invoke undeclared capabilities
- all executions must terminate without cyclic traversal

---

## 9. Workflow model

Workflows are directed acyclic graphs with explicitly typed nodes and transitions.

### 9.1 Why DAGs

The DAG requirement is a deliberate design choice.

It gives `agentd`:

- predictable execution paths
- guaranteed termination of a workflow run
- simpler validation at build time
- easier observability and replay
- easier security review
- flexible modeling of branching and merging without runtime cycles

The system should support **rich DAG topologies**, including:

- multiple start nodes
- parallel branches, if implemented
- conditional branches
- merge points
- distinct terminal nodes

### 9.2 What DAGs do not allow

The workflow graph itself must not contain:

- cycles
- unbounded loops
- arbitrary backward edges
- planner-generated nodes or transitions

If repeated behavior is needed, it must be modeled by:

- a bounded retry policy attached to a node
- a fresh workflow execution on the next external event
- an explicit new start invocation from outside the current execution

### 9.3 Workflow structure

A workflow contains:

- metadata
- zero or more triggers
- one or more start nodes
- nodes
- transitions
- optional execution policies
- optional input schemas
- optional output schemas

### 9.4 Node categories

#### Input/context nodes
- read MCP resource
- read file
- read environment variable
- read HTTP request body
- parse JSON
- load execution state

#### Transformation nodes
- template render
- diff compute
- JSON transform
- data projection
- normalization

#### Intelligence nodes
- LLM inference
- classifier
- extractor
- summarizer
- policy evaluator

#### Action nodes
- write file
- create directory
- delete file
- make HTTP request
- call MCP tool
- emit internal event
- persist state

#### Control nodes
- condition
- switch
- merge
- fail
- terminate

### 9.5 Start nodes

A workflow may have multiple named entry points. Example:

- `on_resource_update`
- `on_http_request`
- `manual_recheck`

This is important because the same workflow DAG may support:

- event-driven execution
- HTTP invocation
- explicit manual execution

without duplicating the graph.

### 9.6 Transition semantics

Transitions may be:

- unconditional
- predicate-based
- output-field-based
- error-path-based

All transitions must be declared in the workflow graph.

### 9.7 No implicit planning

The runtime must not let intelligence invent new nodes or transitions. It may only choose among already-declared branches when that branch point is explicitly configured.

---

## 10. Foundational capability and tool model

The runtime ships with a **foundational tool model**. These tools are not dynamically discovered at runtime. They are part of the Rust project and are selectively included at compile time.

### 10.1 Tool philosophy

Foundational tools must be:

- narrow in purpose
- typed
- policy-checkable
- auditable
- composable inside DAG workflows

The runtime should not expose a vague "do anything" tool surface.

### 10.2 Precompiled foundational tool families

The initial foundational tool families should be:

#### Filesystem tools
- `fs.read_file`
- `fs.write_file`
- `fs.create_file`
- `fs.create_dir`
- `fs.delete_path`
- `fs.list_dir`
- `fs.stat`

#### HTTP tools
- `http.request`

#### Data and utility tools
- `diff.compute`
- `json.parse`
- `json.select`
- `template.render`
- `hash.compute`
- `time.now`

#### Environment tools
- `env.read`

#### MCP tools
- `mcp.subscribe`
- `mcp.read_resource`
- `mcp.call_tool`

#### State and observability tools
- `state.get`
- `state.put`
- `trace.emit`
- `metric.emit`
- `log.emit`

### 10.3 Capabilities that should not exist initially

The initial system should explicitly avoid foundational tools for:

- arbitrary shell command execution
- arbitrary subprocess spawning
- runtime code execution
- dynamic plugin loading from untrusted sources
- unconstrained network access
- self-modifying workflow definitions

Those capabilities materially weaken the bounded model.

### 10.4 Tool selection at compile time

A built `agentd` binary should only include the tool families required by its intended workflows.

This can be achieved through:

- Cargo features
- build-time configuration files
- environment-driven Rust build configuration
- optional code generation that prunes unused tool registrations

The precise mechanism is an implementation detail, but the design requirement is firm: **tool presence must be decided before the binary is built**.

### 10.5 Policy narrowing over compiled tools

Even when a tool family is compiled in, runtime policy may narrow it further.

Examples:

- file reads limited to `/workspace/docs/**`
- writes limited to `/tmp/agent-output/**`
- HTTP requests limited to a known domain allowlist
- environment variable access limited to named keys
- MCP tool invocations limited to specific tool IDs

---

## 11. Build and artifact model

A major feature of `agentd` is the ability to create tightly scoped deployable artifacts.

### 11.1 Build philosophy

The project should not rely on a project-specific operational CLI for building, packing, or defining the agent.

Instead, `agentd` should be built using normal Rust project build flows, with behavior controlled by:

- build configuration files
- embedded workflow/config definitions
- Cargo features
- build arguments
- environment variables used during build
- optional `build.rs` code generation or validation steps

This keeps the system aligned with standard Rust engineering practice and avoids turning the project into a bespoke packaging toolchain.

### 11.2 Supported artifact patterns

#### Mode A: generic runtime + external config
Useful for development and experimentation.

#### Mode B: generic runtime + embedded config
Useful for deployment when a single artifact is desired.

#### Mode C: generated static workflow tables + embedded config
Useful for higher assurance and smaller runtime surface.

#### Mode D: capability-pruned sealed binary
Useful for strict bounded deployment.

### 11.3 Build-time validation

The build process should validate:

- workflow DAG integrity
- acyclicity of every workflow
- existence of start nodes
- trigger references
- schema references
- MCP allowlists
- capability requirements
- prompt template references
- route collisions
- policy consistency

### 11.4 Sealed artifacts

A sealed artifact may contain:

- embedded workflows
- embedded policies
- embedded schemas
- embedded prompt templates
- embedded MCP allowlists
- only the precompiled foundational tool families selected for that build

Secrets should generally not be embedded. References to secret sources may be embedded.

---

## 12. MCP integration model

MCP is a first-class integration surface in `agentd`.

### 12.1 Supported MCP operations

`agentd` should support:

- registering multiple MCP servers
- connecting to configured servers
- enumerating or validating resources/tools if permitted
- subscribing to configured resources or notifications
- reading resources
- invoking allowed MCP tools

### 12.2 MCP authority must be bounded

The runtime must not gain broad MCP power simply because an MCP server exposes it.

Configuration must define:

- which MCP servers are allowed
- which resources may be subscribed to
- which resource URI patterns may be read
- which tools may be called
- optional parameter constraints for tool calls

### 12.3 MCP subscription model

A workflow may register interest in notifications from an MCP resource source.

Example:

- subscribe to `docs://pages/*`
- on update notification, start workflow `review_docs` at start node `on_resource_update`

### 12.4 MCP as trigger and as action

MCP appears in two places:

1. **Trigger/input plane**
   - notifications
   - resource reads

2. **Action plane**
   - tool invocations
   - optional writes if modeled through MCP tools

This symmetry is useful and should be explicit in the architecture.

---

## 13. HTTP trigger model

`agentd` may expose a small HTTP server for configured workflows.

### 13.1 HTTP server purpose

The HTTP interface exists to:

- start specific workflows
- provide a bounded API surface
- expose health/readiness/metrics
- optionally allow controlled replay or test endpoints

### 13.2 HTTP trigger requirements

Each HTTP route must map to:

- exactly one workflow
- exactly one named start node
- an input schema
- an authentication policy
- optional authorization policy

The handler must not dynamically choose arbitrary workflows unless explicitly allowed and tightly constrained.

### 13.3 HTTP input materialization

Request data may be projected into workflow context:

- path parameters
- query parameters
- headers
- body
- authenticated principal
- request ID / trace context

### 13.4 HTTP response model

A workflow may return:

- synchronous response
- accepted/queued response with execution ID
- dry-run explanation

The runtime should not require all workflows to block until completion.

---

## 14. Explicit start-node invocation

`agentd` must support directly starting a workflow from a specific start node.

### 14.1 Why this exists

This supports:

- testing
- replay
- debugging
- manual operations
- controlled direct execution from code or API

### 14.2 Invocation sources

Possible sources:

- internal API
- admin HTTP endpoint
- embedded integration call site
- test harness

### 14.3 Constraints

- target workflow must exist
- start node must be declared
- input must validate against the start node schema
- policy must allow the invocation origin

---

## 15. Intelligence backend model

The intelligence backend is separate from the runtime.

### 15.1 Supported transports

Initial transports:

- Unix domain socket
- HTTP

Later:

- gRPC
- in-process adapter if necessary

### 15.2 Why it is separate

Separating reasoning from the runtime allows:

- local model daemons
- remote model gateways
- enterprise inference services
- smaller trusted runtime
- easier policy and transport control

### 15.3 Intelligence step contract

An intelligence node must declare:

- transport/provider reference
- prompt or instruction template
- input mapping
- output schema
- timeout
- retry policy
- token or size budget if applicable

### 15.4 Intelligence output requirements

Prefer structured outputs only. Example:

```json
{
  "decision": "post_comment",
  "confidence": 0.94,
  "comment": "The update introduces a version mismatch in the deployment steps."
}
```

The runtime validates this against schema before any downstream side effect.

### 15.5 Intelligence authority

The intelligence backend may recommend or classify, but it does not define new capabilities, create new graph edges, or authorize side effects outside declared policy.

---

## 16. Policy model

Policy is enforced by the runtime, not by the prompt.

### 16.1 Policy layers

#### Compile-time policy
What the binary can fundamentally do.

#### Configuration policy
What this deployment intends the binary to do.

#### Runtime policy
What is valid in this execution given current context.

#### Environment policy
What the OS/container/sandbox permits.

### 16.2 Examples

- only read files under `/workspace/docs`
- only write under `/tmp/agent-output`
- only call `POST` on `https://api.internal.example`
- only subscribe to `docs://pages/*`
- only call MCP tool `comment_on_page`
- only read env vars `GITHUB_TOKEN`, `DOCS_ROOT`

### 16.3 Fail-closed behavior

When a policy check fails:

- the action must not execute
- an audit record must be emitted
- the workflow may terminate or follow a declared error edge

---

## 17. Configuration model

The configuration defines the agent's bounded behavior. It may be external or embedded.

### 17.1 Configuration sections

A typical config should include:

- agent metadata
- enabled capability families
- intelligence backends
- MCP servers
- HTTP server options
- workflows
- policies
- observability settings
- environment variable bindings

### 17.2 Example configuration sketch

```toml
name = "agent"
version = "0.1.0"

[build]
embed_config = true

[capabilities]
fs_read = true
fs_write = true
fs_create_dir = true
fs_delete = false
http_request = true
diff_compute = true
mcp = true
env_read = ["DOCS_ROOT", "API_TOKEN"]

[intelligence.default]
transport = "unix"
endpoint = "/run/intelligence.sock"
timeout_ms = 8000

[http]
enabled = true
bind = "127.0.0.1:8080"

[[mcp.servers]]
name = "docs"
url = "http://127.0.0.1:9001/mcp"
allowed_resources = ["docs://pages/*"]
allowed_tools = ["comment_on_page"]
subscribe_resources = ["docs://pages/*"]

[[workflows]]
name = "document_review"

[[workflows.start_nodes]]
name = "on_resource_update"
source = "event"

[[workflows.start_nodes]]
name = "on_http_request"
source = "http"

[[workflows.start_nodes]]
name = "manual_review"
source = "manual"

[[workflows.triggers]]
type = "mcp.resource.updated"
server = "docs"
resource = "docs://pages/*"
start_node = "on_resource_update"

[[workflows.http_routes]]
method = "POST"
path = "/workflows/document-review"
start_node = "on_http_request"
input_schema = "schemas/review_request.json"

[[workflows.nodes]]
id = "load_resource"
type = "read_mcp_resource"
resource_from = "trigger.resource_uri"

[[workflows.nodes]]
id = "analyze"
type = "llm_infer"
backend = "default"
input_from = "load_resource"
prompt = "Analyze the updated document and classify whether a comment is needed."
output_schema = "schemas/review_decision.json"

[[workflows.nodes]]
id = "decision"
type = "switch"
expr = "analyze.decision"

[[workflows.nodes]]
id = "post_comment"
type = "call_mcp_tool"
tool = "comment_on_page"
args_from = "analyze.comment_payload"

[[workflows.nodes]]
id = "done"
type = "terminate"

[[workflows.edges]]
from = "load_resource"
to = "analyze"

[[workflows.edges]]
from = "analyze"
to = "decision"

[[workflows.edges]]
from = "decision"
when = "comment"
to = "post_comment"

[[workflows.edges]]
from = "decision"
when = "ignore"
to = "done"

[[workflows.edges]]
from = "post_comment"
to = "done"
```

---

## 18. Runtime state model

`agentd` should remain lightweight, but some state is useful.

### 18.1 Execution-local state
Always present; scoped to one execution.

### 18.2 Durable state
Optional. Useful for:

- deduplication
- replay checkpoints
- simple correlation
- last-seen resource version
- idempotency keys

### 18.3 State design constraints

State should not turn `agentd` into a general memory-heavy agent system. Keep state narrow and explicit.

---

## 19. Error handling model

Errors are first-class execution outcomes.

### 19.1 Error categories

- configuration error
- policy violation
- capability unavailable
- MCP connection failure
- resource read failure
- tool invocation failure
- invalid intelligence output
- timeout
- auth failure
- schema validation failure

### 19.2 Workflow behavior on error

Each node may define:

- fail execution
- transition to error node
- retry with backoff
- skip and continue, only where safe
- emit alert/audit and terminate

### 19.3 Idempotency

For workflows with side effects, the runtime should encourage idempotent actions or explicit idempotency keys.

---

## 20. Observability model

Observability is a core feature, not an afterthought.

### 20.1 Logging

Structured logs should include:

- execution ID
- workflow ID
- start node
- trigger type
- node ID
- outcome
- latency
- policy decision summary

### 20.2 Tracing

OpenTelemetry tracing should include:

- workflow execution span
- per-node spans
- intelligence request span
- MCP request span
- HTTP request span
- policy evaluation events

### 20.3 Metrics

Useful metrics include:

- workflow starts/completions/failures
- node execution latency
- intelligence latency/error rate
- MCP subscription health
- HTTP request counts
- policy denials
- retries/timeouts
- active executions

### 20.4 Audit trail

Audit records should capture:

- who or what triggered the workflow
- what decision was made
- what side effects were attempted
- whether policy allowed them
- what inputs were redacted or hashed

---

## 21. Security considerations

### 21.1 Principle of least authority
The binary should only include capabilities it needs.

### 21.2 Path and domain restrictions
Filesystem and network should be constrained by allowlists.

### 21.3 MCP restrictions
Never treat MCP discovery as authorization.

### 21.4 No arbitrary shell by default
A generic shell/subprocess tool defeats the bounded model and should not exist in the initial system.

### 21.5 Prompt injection resistance
Because control flow is declared and policies are enforced by runtime, prompt injection impact is reduced, though not eliminated. Intelligence outputs must still be schema-validated and policy-checked.

### 21.6 Sandboxing
`agentd` should run well inside containers, chroots, namespaces, or host-level sandboxing if stronger isolation is desired.

### 21.7 Auth and authz for HTTP
HTTP-triggered workflows must support authentication and route-level authorization.

---

## 22. Testing model

Testing must be a first-class workflow experience.

### 22.1 Fixture-driven tests

Developers should be able to define:

- input trigger fixture
- expected graph path
- mocked tool responses
- mocked intelligence output
- expected side effects

### 22.2 Dry-run mode

Dry-run mode should:

- execute graph logic
- evaluate guards and policies
- mock or suppress side effects
- emit detailed execution explanation

### 22.3 Replay mode

Replay mode should allow:

- reusing recorded triggers
- reusing stored execution context
- reproducing failures
- comparing old vs new workflow behavior

### 22.4 Manual start-node testing

A direct way to invoke:

- workflow = `document_review`
- start node = `manual_review`
- input = fixture JSON

This will be extremely useful.

---

## 23. Architecture overview

### 23.1 Main components

#### Core runtime
- execution engine
- DAG traversal
- context/state
- policy enforcement

#### Capability modules
- filesystem
- HTTP
- diff
- env
- MCP

#### Intelligence adapter
- Unix socket client
- HTTP client
- schema validation
- prompt rendering

#### Trigger adapters
- MCP subscription listener
- HTTP server
- programmatic/manual invoker

#### Observability
- logs
- traces
- metrics
- audit emitter

#### Build subsystem
- parser
- validator
- artifact builder
- optional code generator

### 23.2 Suggested Rust crate layout

- `agent-core`
- `agent-workflow`
- `agent-policy`
- `agent-intelligence`
- `agent-mcp`
- `agent-http`
- `agent-tools`
- `agent-observability`
- `agent-build-support`

*Implementation note (2026-04-22):* MVP keeps a single crate at
`crates/agentd/` with internal `mod` split. Promoting seams
into separate crates happens once the module boundaries have
stabilised.

---

## 24. Example workflow patterns

### 24.1 Resource-change reviewer
Trigger on MCP notification, read full resource, classify, optionally comment.

### 24.2 HTTP request classifier
Accept JSON request, run extraction/classification, forward result via HTTP or file write.

### 24.3 Manual remediation checker
Operator or integrating system invokes start node directly, reads file/resource, performs bounded analysis, emits recommendation.

### 24.4 Notification router
Receive inbound event, classify destination, call one allowed MCP tool or internal HTTP endpoint.

---

## 25. Open questions

The RFC should explicitly leave these for later refinement:

1. Should the DAG model support explicit parallel branches in v1 or only sequential and conditional edges?
2. Should internal workflow-to-workflow invocation be part of v1?
3. Should durable state be embedded or pluggable?
4. Should HTTP trigger mode support async queueing by default?
5. Should prompt templates be embedded only, or overridable?
6. Should we support signed configs and signed artifacts?
7. Should generated-code mode be introduced in MVP or after the generic engine?
8. How strict should the build-time pruning of foundational tools be in the first release?

---

## 26. MVP scope

The first implementation should stay small.

### 26.1 In scope

- Rust daemon/runtime
- config-driven workflows modeled as DAGs
- three trigger modes:
  - event/notification
  - HTTP
  - explicit start-node invocation
- MCP integration:
  - connect
  - subscribe
  - read resource
  - call tool
- limited foundational tools:
  - read/write file
  - create directory
  - HTTP request
  - diff
  - env read
- intelligence over Unix socket or HTTP
- structured output validation
- tracing/logging/metrics
- health endpoint
- dry-run mode
- build-time validation of workflow DAGs and capabilities

### 26.2 Out of scope for MVP

- dynamic plugin system
- arbitrary runtime tool loading
- unrestricted subprocess execution
- long-lived memory/planning
- generalized multi-agent orchestration
- UI control plane
- cyclic workflows
- timer/scheduler support unless trivial

---

## 27. Future extensions

After MVP, the project could add:

- signed sealed artifacts
- generated-code workflow compilation
- richer policy language
- replay storage
- workflow versioning and migration
- timer and cron triggers
- bounded parallel execution in DAG branches
- secure secrets providers
- distributed coordination only if truly needed
- WASI packaging in constrained environments

---

## 28. Rationale for the name

The project is intentionally named **`agentd`**, but architecturally it is not a general agent runtime. The name is acceptable so long as the documentation repeatedly clarifies that:

- it is bounded
- it is workflow-driven
- it is compile-time constrained
- it is not an autonomous planner

Internally, the project should stay disciplined about that boundary.

---

## 29. Final definition

`agentd` is a Rust runtime for executing predeclared, policy-constrained intelligence workflows defined as directed acyclic graphs and triggered by events, HTTP requests, or explicit start-node invocation. It integrates with MCP, supports a small precompiled foundational tool surface, uses intelligence only as a bounded reasoning step, and emphasizes auditability, safety, and operational predictability over autonomy.
