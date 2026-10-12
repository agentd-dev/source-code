# Events and metrics reference

This page lists what a running agentd writes about itself: the JSON-lines
log events on stderr, the Prometheus metrics on `/metrics`, the health
surfaces, the OTLP export and the audit record. It is for operators building
dashboards and alerts, and for integrators parsing the stream. It covers a
binary built with every release feature; to learn how to use these signals,
read [Monitor in production](observability.md).

## Log line format

agentd writes one JSON object per line to stderr. stdout carries only a
job's result. Every line starts from the same envelope, and the event's own
fields are merged into it.

| Field | Always | Meaning |
|---|---|---|
| `ts` | yes | RFC 3339 UTC with milliseconds, for example `2026-10-11T18:35:54.388Z` |
| `level` | yes | `trace`, `debug`, `info`, `warn` or `error` |
| `event` | yes | The dotted event name, for example `run.done` |
| `run_id` | yes | The instance's run id: `--run-id`, or a ULID minted at start. Every process in the tree carries the same value. |
| `agent_id` | yes | `sup` for the supervisor; for a child, the same value as its `agent_path` |
| `agent_path` | yes | Which unit of work wrote the line (see below) |
| `comp` | yes | `supervisor` for the supervisor process, `agent` for turn workers and subagents |
| `pid` | yes | The OS process id, which joins the log to `ps` and `pstree` |
| `trace_id` | yes | The W3C trace id: adopted from `observability.traceparent`, else derived from `run_id` |

Rules a parser can rely on:

- Keys are written in alphabetical order. Parse by name, not position.
- An event's own field may replace an envelope key. `turn.model` carries
  the `agent_path` of the turn it describes, and an `audit` line's `ts` is
  the record's timestamp in milliseconds since the epoch, a number.
- `err` is a string.
- Levels below `observability.log_level` (default `info`) are not written.
  `debug` adds `turn.round`, `intel.call`, `intel.result`, `loop.step`,
  `tool.reply` and the A2A connection lines.
- The `--validate-config` verdict lines (`config.valid`, `config.invalid`)
  carry no envelope. They are the tool's answer, not log events.

A supervisor line and a turn worker line from the same run:

```json
{"acting_for":"system","agent_id":"sup","agent_path":"0","comp":"supervisor","event":"run.start","inbox_event":"01M4P4004T51G3953PXYBDCPQS","key":null,"level":"info","node":"start","pid":1845135,"run":"main-01M4P4004VPT9M256ZXBGDGV3C","run_id":"01M4P4004MMJKM81HAZA0JFGP4","trace_id":"f281fdc06f4dd1374e53be619f96e1fe","ts":"2026-10-11T18:35:54.395Z","workflow":"main"}
{"agent_id":"run/main-01M4P4004VPT9M256ZXBGDGV3C/work","agent_path":"run/main-01M4P4004VPT9M256ZXBGDGV3C/work","comp":"agent","event":"turn.done","level":"info","pid":1845136,"rounds":1,"run_id":"01M4P4004MMJKM81HAZA0JFGP4","status":"completed","tokens_in":11,"tokens_out":5,"tool_calls":0,"trace_id":"f281fdc06f4dd1374e53be619f96e1fe","ts":"2026-10-11T18:35:54.412Z","turn":"main-01M4P4004VPT9M256ZXBGDGV3C/work#1"}
```

### agent_path values

`agent_path` names the unit of work, so a prefix selects all of it without a
join.

| Value | Written by |
|---|---|
| `0` | The supervisor |
| `run/<run>/<step>` | The turn worker of one workflow step |
| `turn/<ctx>` | The turn worker of one conversation turn |
| `sub/<handle>` | A subagent |
| `preflight/<ctx>` | Preflight triage of an inbound message |
| `compact/<ctx>` | Compaction of a conversation |
| `decide/<ctx>` | A `decide` call |
| `think/<ctx>` | A `think` call |

`<run>` is the workflow run id (`<workflow>-<ULID>`), `<ctx>` a conversation
context id, `<handle>` a subagent handle.

## Event families

An event's family is the segment before its first dot. The family list is
closed: `observability.runtime_events.include` and `.sampled` accept only
these names, and anything else is refused at load.

| Family | Covers |
|---|---|
| `a2a` | The A2A listener, peers, push notifications and tasks |
| `admin` | `admin.set` |
| `agent` | An instance-wide pause |
| `audit` | Audit records written to the log, and audit write failures |
| `auth` | Display-client launch codes and sign-in sessions |
| `breaker` | Step circuit breakers |
| `budget` | Token budgets: windows, degrade, the lifetime cap |
| `cgroup` | cgroup v2 placement of children |
| `child` | Child process exits and health |
| `config` | Validation, reload and the config file watch |
| `context` | Conversation compaction |
| `correlate` | `correlate` start nodes |
| `drain` | Graceful shutdown |
| `exec` | The `exec` tool |
| `freshness` | The instruction freshness watch |
| `goal` | The goal watchdog |
| `http` | The `http` workflow node |
| `human` | Questions to people, answers and judges |
| `identity` | Principal id collisions |
| `inbox` | The durable inbox |
| `instance` | Instance-tier subagents |
| `instruction` | The instruction source |
| `intel` | Model calls, decisions, health and model swaps |
| `knowledge` | Automatic knowledge context |
| `lifecycle` | Idle exit and the retirement signal |
| `limit` | Reserved; nothing writes it |
| `loop` | A subagent's agentic loop |
| `mcp` | MCP connections, listens, subscriptions and MCP Events |
| `message` | The message-depth guard |
| `metrics` | The probe and scrape listener |
| `nested` | Fan-out bodies |
| `otel` | OTLP log export |
| `plan` | Plan updates |
| `preflight` | Preflight triage |
| `pressure` | Resource pressure |
| `proc` | Process start, readiness and exit |
| `prompt` | The system prompt and `--prompt` |
| `readback` | Results withheld from a tainted reader |
| `registry` | The tool registry |
| `restore` | Restoring durable state at start |
| `root` | The root conversation |
| `run` | Workflow runs |
| `signal` | Workflow signals |
| `skill`, `skills` | Skills discovery and loading |
| `start` | Start nodes |
| `step` | Workflow steps |
| `store` | The durable store |
| `stream` | Streams and the runtime-events tap |
| `subagent` | Subagents |
| `test` | Test hooks, compiled only into debug and `internal-mocks` builds, never into a release binary |
| `timer` | Durable timers |
| `tool` | Tool calls and tool policies |
| `turn` | Turn workers |
| `wait` | Suspended waits |
| `webhook`, `webhooks` | Webhook routes and the webhook listener |
| `workflow` | Workflow definitions |

## Events

Every event agentd writes, by family. **Fields** are the event's own
keys beyond the envelope; a `?` marks a key that is present only sometimes.
"Stream fields" are the keys a `stream` or `correlate` consumer adds to say
which events it holds: `stream`, `seq` and `event_id` for one event; `from`,
`to`, `events`, `seqs` and `event_ids` for a batch; `correlation`, `events`,
`seqs` and `event_ids` for a join. Levels separated by a comma depend on the
case.

### a2a

| Event | Level | Fields | Notes |
|---|---|---|---|
| `a2a.conn` | debug | `err` | A connection could not be served. |
| `a2a.context.unavailable` | warn | `err`, `principal` |  |
| `a2a.delegate` | info | `peer`, `run`, `step` |  |
| `a2a.denied` | warn | `principal`, `rule`, `method`, `op`, `reason`, `status`, `sid?`, `suppressed?` | The listener refused a request. `reason`: `unauthenticated`, `invalid_credential`, `no_role`, `not_permitted`, `rate_limited`, `auth_failures_limited`. Refusals a stranger can cause are written once per source and reason per minute; `suppressed` counts the rest. |
| `a2a.introspection` | info | `enabled` |  |
| `a2a.listen` | info | `authority`, `bound`, `device_grant`, `events`, `implicit_operator`, `introspection`, `mtls`, `required`, `tls`, `url` | The listener bound, with the posture it enforces. |
| `a2a.message.delivered` | info | `conversation`, `inbox_event` |  |
| `a2a.push.failed` | warn | `err`, `task` |  |
| `a2a.push.registered` | info | `config`, `principal`, `task` |  |
| `a2a.send` | info | `run`, `step`, `to` |  |
| `a2a.task.encode.fail` | warn | `err`, `task` |  |
| `a2a.task.evict.fail` | warn | `err`, `task` |  |
| `a2a.task.evicted` | info | `task` |  |
| `a2a.task.persist.fail` | warn | `err`, `task` |  |
| `a2a.tls` | debug | `err` |  |
| `a2a.unix.denied` | warn | `err`, `reason`, `uid` | A unix-socket peer was refused. |

### admin

| Event | Level | Fields | Notes |
|---|---|---|---|
| `admin.set` | info | `path`, `value` | An operator changed a runtime-settable path. |

### agent

| Event | Level | Fields | Notes |
|---|---|---|---|
| `agent.paused` | info | `reason` | An instance-wide `admin.pause` began. |
| `agent.resumed` | info | — | The instance-wide pause ended. |

### audit

| Event | Level | Fields | Notes |
|---|---|---|---|
| `audit` | info | `instance`, `principal`, `role`, `action`, `target`, `outcome`, `request_id`, `trace`, `sid?` | One audit record, when `observability.audit.sink` includes `log`. |
| `audit.emit` | info | `audit`, `run`, `step` | An `emit` step wrote an `audit` record. |
| `audit.store.fail` | warn | `action`, `err` | An audit record could not be stored. The audited action still stands. |

### auth

| Event | Level | Fields | Notes |
|---|---|---|---|
| `auth.launch.approved` | info | `client_id` |  |
| `auth.launch.exchanged` | info | `bind`, `client_id`, `sid`, `via` |  |
| `auth.launch.requested` | info | `client_id` |  |
| `auth.session.revoked` | info | — |  |

### breaker

| Event | Level | Fields | Notes |
|---|---|---|---|
| `breaker.closed` | info | `breaker` | The probe succeeded; the breaker closed. |
| `breaker.open` | warn | `breaker`, `consecutive_failures`, `cooldown` | A step circuit breaker opened after consecutive failures. |
| `breaker.probe` | info | `breaker`, `run` | A run took the half-open probe slot. |
| `breaker.reopen` | warn | `breaker`, `probe_failed` | The probe failed; the breaker opened again. |

### budget

| Event | Level | Fields | Notes |
|---|---|---|---|
| `budget.degraded` | info | `ctx`, `model`, `run`, `step` | A step was moved to the budget's degrade model. |
| `budget.lifetime_exhausted` | warn | `policy`, `reason` | The instance lifetime token budget is spent; `policy` says what follows. Written once. |
| `budget.refused` | warn | `ctx`, `reason` | A turn was refused by a token budget. |
| `budget.wait` | info | `ctx`, `node`, `reason`, `run`, `step`, `until_ms` | A step waits for a budget window to reset. |

### cgroup

| Event | Level | Fields | Notes |
|---|---|---|---|
| `cgroup.armed` | info | `limits_unavailable`, `parent` | cgroup v2 placement for children is set up. |

### child

| Event | Level | Fields | Notes |
|---|---|---|---|
| `child.exit` | info | `kind`, `node`, `outcome`, `pid` | A child process was reaped. `outcome` is `Exited(<code>)` or a signal. |
| `child.unhealthy` | warn | `health`, `node` | A child was found unhealthy and is being stopped. |

### config

| Event | Level | Fields | Notes |
|---|---|---|---|
| `config.invalid` | error | `error` | A validation error at startup. |
| `config.reload.invalid` | warn | `error`, `trigger` | A reload candidate failed validation or staging; nothing changed. |
| `config.reload.restart_required` | warn | `paths`, `trigger` | A reload touched restart-only `paths`; nothing changed. |
| `config.reloaded` | info | `changed`, `trigger` | A reload was applied. `changed` lists what moved, or `["nothing"]`. |
| `config.warning` | warn | `warning` | A non-fatal finding at load or reload. |
| `config.watch.armed` | info | `dir`, `glob`, `path` | The `lifecycle.watch_config` watch is set on a directory. |
| `config.watch.error` | warn | `dir`, `err`, `errno`, `path`, `target` | The watch failed and stopped; `SIGHUP` still reloads. |
| `config.watch.fired` | info | `file` | A watched file changed; a reload follows. |

### context

| Event | Level | Fields | Notes |
|---|---|---|---|
| `context.compacted` | info | `after`, `before`, `ctx`, `fallback`, `folded`, `version` | A conversation was compacted. |
| `context.compaction.apply_fail` | warn | `ctx`, `err` |  |
| `context.compaction.spawn_fail` | warn | `ctx`, `err` |  |
| `context.compaction.start` | info | `ctx`, `fold`, `node` |  |
| `context.compaction.think_failed` | warn | `ctx`, `err`, `status` |  |

### correlate

| Event | Level | Fields | Notes |
|---|---|---|---|
| `correlate.expired` | info | `correlation`, `node`, `stream`, `window_ms`, `workflow` |  |
| `correlate.overflow` | warn | `dropped`, `max_pending`, `node`, `stream`, `workflow` | A `correlate` start refused new correlation values past `max_pending`. |
| `correlate.unkeyed` | warn | `by`, `node`, `stream`, `subject`, `workflow` |  |

### drain

| Event | Level | Fields | Notes |
|---|---|---|---|
| `drain.abandon` | warn | `children` | The drain overran `drain_timeout` plus 3 s; the remaining children were killed. |
| `drain.done` | info | `reason` | The drain finished; the process exits next. |
| `drain.start` | info | `children`, `reason`, `runs` | A graceful drain began (`SIGTERM`, `SIGINT`, `admin.drain` or a policy). |

### exec

| Event | Level | Fields | Notes |
|---|---|---|---|
| `exec.run` | info | `argc`, `caller`, `cmd`, `cwd`, `timeout_ms` | The `exec` tool ran a command. |

### freshness

| Event | Level | Fields | Notes |
|---|---|---|---|
| `freshness.armed` | info | `every_ms` | The instruction freshness watch is armed. |
| `freshness.recovered` | info | `uri` |  |

### goal

| Event | Level | Fields | Notes |
|---|---|---|---|
| `goal.achieved` | info | `reason`, `statement`, `via` | The goal check says the goal holds. |
| `goal.armed` | info | `every_ms`, `statement`, `stuck_after`, `via` |  |
| `goal.check` | info | `achieved`, `judge`, `no_progress` |  |
| `goal.escalate` | warn | `reason`, `statement` |  |
| `goal.idle` | info | `note`, `reason` |  |
| `goal.judge` | info | `achieved`, `reason` |  |
| `goal.judge.error` | warn | `error` |  |
| `goal.judge.start` | info | `no_progress`, `statement` |  |
| `goal.replan` | warn | `note`, `reason`, `statement` |  |
| `goal.stuck` | warn | `no_progress`, `reason`, `statement`, `stuck_after`, `via` | The goal made no progress for `stuck_after` checks. |
| `goal.workflow` | info | `reason`, `workflow` |  |
| `goal.workflow.missing` | warn | `note`, `workflow` |  |

### http

| Event | Level | Fields | Notes |
|---|---|---|---|
| `http.request` | info | `method`, `run`, `step`, `url` | An `http` workflow node made a request. |

### human

| Event | Level | Fields | Notes |
|---|---|---|---|
| `human.answer.not_addressed` | info | `addressee`, `from`, `task` |  |
| `human.answer.override` | warn | `addressee`, `by`, `sid`, `task` | An operator answered a gate addressed to someone else. |
| `human.answer.rejected` | info | `errors`, `task`, `via` |  |
| `human.answer.undelivered` | warn | `task`, `via` |  |
| `human.answered` | info | `by`, `labels`, `sid`, `task`, `via` | A person answered. |
| `human.ask` | info | `channel`, `deadline_ms`, `question`, `task` | A question went to a person. |
| `human.ask.failed` | warn | `err`, `task` |  |
| `human.ask.parked` | info | `ask`, `deadline_ms`, `note` |  |
| `human.ask.pruned` | warn | `err`, `task` |  |
| `human.ask.restored` | info | `run`, `step`, `task` |  |
| `human.auto_accepted` | info | `answer`, `policy`, `question` | `agent.approval: accept` took an ask's recommended answer. |
| `human.judge.start` | info | `ask`, `decided`, `question` |  |
| `human.judge.withheld` | warn | `ask`, `origins`, `why` | No judge answered: the asking run carries outside input. The ask fails. |

### identity

| Event | Level | Fields | Notes |
|---|---|---|---|
| `identity.collision` | error, warn | — | A reload declared a principal id an approved device already owns; the reload is refused. |

### inbox

| Event | Level | Fields | Notes |
|---|---|---|---|
| `inbox.accepted` | info | `inbox_event`, `kind` | An event was written to the durable inbox. |
| `inbox.done.fail` | warn | `err`, `inbox_event` |  |
| `inbox.unknown_kind` | warn | `inbox_event`, `kind` |  |

### instance

| Event | Level | Fields | Notes |
|---|---|---|---|
| `instance.exited` | info | `handle`, `pid`, `status` |  |
| `instance.kill` | warn | `handle`, `note`, `pid` |  |
| `instance.metered` | info | `delta_tokens`, `handle`, `total_tokens` |  |
| `instance.mirror` | info | `handle`, `seq`, `stream` |  |
| `instance.mirror.fail` | warn | `err`, `handle`, `stream` |  |
| `instance.op.fail` | warn | `err`, `op` |  |
| `instance.op.orphan` | warn | `handle`, `note`, `op` |  |
| `instance.op.refused` | warn | `op`, `principal`, `note` | A non-operator tried an instance operation. |
| `instance.op.unknown` | warn | `op` |  |
| `instance.parent_peer` | warn | `note` |  |
| `instance.respawn` | info | `handle`, `pid` |  |
| `instance.result` | info | `handle` |  |
| `instance.retire` | info | `handle`, `pid`, `why` |  |
| `instance.send` | info, warn | `handle`, `err?` |  |
| `instance.spawn` | info | `handle`, `pid`, `retire_at`, `singleton`, `socket`, `template`, `until` | An instance-tier subagent started. |
| `instance.spawn.refused` | warn | `reason`, `template` | An instance-tier subagent was refused before anything was created. |

### instruction

| Event | Level | Fields | Notes |
|---|---|---|---|
| `instruction.applied` | info | `delivered_digest`, `new_version_id`, `old_version_id`, `uri` |  |
| `instruction.binding.created` | info | `binding_id`, `instruction_id`, `server` |  |
| `instruction.binding.fail` | warn | `err`, `op`, `server` |  |
| `instruction.binding.reported` | info | `binding_id`, `version_id` |  |
| `instruction.event` | info | `event_type`, `server` |  |
| `instruction.loaded` | info | `bytes`, `dir`, `files`, `glob`, `layer_digest`, `manifest_digest`, `order`, `resolution`, `server`, `uri`, `version`, `version_id` | The instruction was read. |
| `instruction.reread.fail` | warn | `err`, `reason`, `server`, `uri` |  |
| `instruction.subscribe.fail` | warn | `err`, `server`, `uri` |  |
| `instruction.subscribe.unsupported` | info | `server`, `uri` |  |
| `instruction.unavailable` | warn | `err`, `policy`, `server`, `trust_pinned`, `uri` | The instruction source could not be re-read before its deadline; `policy` is what agentd did. |
| `instruction.updated` | info | `version` | A changed instruction took effect. |
| `instruction.verified` | info | `author_capabilities`, `capabilities`, `delivery_checked`, `key_state`, `kid`, `publisher`, `uri` | A signed instruction verified against a trusted publisher. |

### intel

| Event | Level | Fields | Notes |
|---|---|---|---|
| `intel.call` | debug | `messages`, `step` | A subagent loop asked the model. |
| `intel.decision` | info | `answers`, `escalated`, `model`, `turn`, `unsure` | A decision model or judge answered. |
| `intel.decision.escalate` | info | `from`, `to`, `turn`, `unsure` | An unsure decision was asked again on the `escalate` model. |
| `intel.health` | warn | `all_down` | A child reported every model endpoint down, or one back up. Drives `/readyz`. |
| `intel.result` | debug | `tokens_in`, `tokens_out`, `tool_calls` | A subagent loop got the model's answer. |
| `intel.swap` | info | `endpoint_change`, `kind`, `model_from`, `model_to`, `policy` | A model or endpoint change applied at a turn boundary. Names models, never URLs or credentials. |
| `intel.swap.reject` | warn | `err` | A pending swap was refused at the turn boundary. |
| `intel.swap.restart_turn` | info | `discarded_turn` |  |
| `intel.swap.sent` | info | `policy`, `subagents` | A reload's model change was sent to running subagents. |
| `intel.token_file.error` | warn | `err`, `provider` | A provider's token file could not be read. |

### knowledge

| Event | Level | Fields | Notes |
|---|---|---|---|
| `knowledge.auto_context` | info | `bytes`, `ctx`, `hit` |  |
| `knowledge.auto_context.args` | warn | `err` |  |

### lifecycle

| Event | Level | Fields | Notes |
|---|---|---|---|
| `lifecycle.idle_exit` | info | `code`, `job_shape` | A job (or an idle instance) is exiting with `code`. |
| `lifecycle.until_signal` | info | `signal` | The `lifecycle.until_signal` signal arrived; a drain follows. |
| `lifecycle.until_signal.refused` | warn | `note`, `principal`, `signal` | That signal came from a sender that may not retire the instance. |

### loop

| Event | Level | Fields | Notes |
|---|---|---|---|
| `loop.error` | error | `err` | A subagent loop failed. |
| `loop.final` | info, warn | `status`, `steps`, `tokens` | A subagent loop ended. |
| `loop.paused` | info | — |  |
| `loop.resumed` | info | — |  |
| `loop.start` | info | `depth`, `max_steps`, `resources`, `servers`, `tools`, `warm` | A subagent loop started. |
| `loop.step` | debug | `messages`, `step`, `tokens` |  |

### mcp

| Event | Level | Fields | Notes |
|---|---|---|---|
| `mcp.connect` | info | `server`, `tools`, `protocol`, `reason?` | A server connected. `protocol` is the negotiated MCP revision. `reason` marks a reload, a re-dial or a join. |
| `mcp.connect.fail` | warn | `server`, `err`, `reason?` | A server could not be connected. Not fatal: agentd retries on a backoff. |
| `mcp.disconnect` | info, warn | `reason`, `server` | A connection closed. `reason`: `reload`, `reload refused`, or `session_lost` (re-dialed). |
| `mcp.events.arm.fail` | error, warn | `err`, `event_type`, `retry_ms`, `server` |  |
| `mcp.events.armed` | info | `server`, `event_type`, `mode`, `cursor_present`, `truncated?` | An MCP Events binding started polling, or its webhook subscription was made. |
| `mcp.events.callback_failed` | error | `event_type`, `reason`, `server`, `url` | The server could not reach or verify `webhooks.url`. |
| `mcp.events.delivery_suspended` | warn | `active`, `event_type`, `last_error`, `server` |  |
| `mcp.events.gap` | warn | `server`, `event_type`, `cause`, `refused?` | Occurrences may be missing. `cause`: `truncated`, `no_replay`, `schema_changed`. |
| `mcp.events.hook.denied` | warn | `event_type`, `reason`, `server` | A webhook delivery did not verify and was answered 401. |
| `mcp.events.hook.unsaved` | warn | `err` |  |
| `mcp.events.index.failed` | warn | `err`, `subscriptions` |  |
| `mcp.events.list.fail` | warn | `err`, `retry_ms`, `server` |  |
| `mcp.events.list_changed` | info | `server` |  |
| `mcp.events.listed` | info | `server`, `types` |  |
| `mcp.events.mode_changed` | warn | `cause`, `event_type`, `from`, `server`, `to` |  |
| `mcp.events.paused` | warn | `server`, `event_type`, `cause`, `detail?` | A binding is held at its cursor. `cause`: `pressure`, `store_degraded`, `rate`, `refusal`. |
| `mcp.events.poll.fail` | error, warn | `server`, `event_type`, `err`, `code?`, `retry_ms` |  |
| `mcp.events.refused` | error | `detail`, `event_type`, `reason`, `server` | A binding was refused and is not polled until a reload, re-dial or new listing allows it. |
| `mcp.events.rejected` | warn | `cause`, `event_id`, `event_type`, `server` |  |
| `mcp.events.removed` | info | `event_type`, `reason`, `server` |  |
| `mcp.events.resumed` | info | `cause`, `event_type`, `server` |  |
| `mcp.events.schema_mismatch` | warn | `event_id`, `event_type`, `server` |  |
| `mcp.events.subscribed` | info | `server`, `event_type`, `owner`, `reason`, `subscription` |  |
| `mcp.events.terminated` | error | `code`, `event_type`, `server` | The server ended a webhook subscription. |
| `mcp.events.throttled` | info | `event_type`, `retry_after_ms`, `server` |  |
| `mcp.events.unsubscribed` | info | `server`, `event_type`, `owner`, `reason`, `subscription` |  |
| `mcp.join.refused` | warn | `errors`, `server` | A server's tools could not join the registry; it is dialed again on a backoff. |
| `mcp.joined` | info | `server` | A server that was down at boot came up and its tools joined. |
| `mcp.listen.ended` | info, warn | `server`, `reason`, `detail?`, `retry_ms` | A `subscriptions/listen` stream ended and reopens after `retry_ms`. Info when idle, warn otherwise. |
| `mcp.listen.flags_refused` | warn | `server`, `reason` |  |
| `mcp.listen.narrowed` | warn | `server`, `dropped` | A re-listen came back without some URIs; they are asked for again. |
| `mcp.listen.resumed` | info | `server` |  |
| `mcp.listen.unavailable` | warn | `server`, `reason`, `retry_ms` | The standing listen failed or was refused. |
| `mcp.tool.dropped` | warn | `reason`, `server`, `tool` | A tool the server listed was dropped by the client and cannot be called. |
| `mcp.tools_changed` | info | `note`, `server` | A server said its tool list changed. |
| `mcp.tools_refresh_failed` | warn | `err` |  |
| `mcp.tools_refreshed` | info | `tools` |  |
| `mcp.unsubscribe.fail` | warn | `err`, `reason`, `server`, `uri` |  |
| `mcp.unsubscribed` | info | `reason`, `server`, `uri` | A resource nobody wants any more was released. |

### message

| Event | Level | Fields | Notes |
|---|---|---|---|
| `message.too_deep` | warn | `caller`, `conversation`, `depth`, `max`, `reason`, `run`, `step` | A message chain passed `limits.max_message_depth`. |

### metrics

| Event | Level | Fields | Notes |
|---|---|---|---|
| `metrics.bound_all_interfaces` | warn | `addr`, `note` | The probe listener is reachable off-host. |
| `metrics.serve.fail` | warn | `addr`, `err` | The probe listener could not bind. Not fatal. |
| `metrics.serving` | info | `addr`, `endpoints` | The `/metrics`, `/healthz` and `/readyz` listener is up. |

### nested

| Event | Level | Fields | Notes |
|---|---|---|---|
| `nested.items.artifact_fail` | warn | `err`, `run`, `step` |  |

### otel

| Event | Level | Fields | Notes |
|---|---|---|---|
| `otel.logs.armed` | info | `endpoint` | OTLP log export is on. |

### plan

| Event | Level | Fields | Notes |
|---|---|---|---|
| `plan.updated` | info | `ctx`, `items`, `op`, `progress` |  |

### preflight

| Event | Level | Fields | Notes |
|---|---|---|---|
| `preflight.failed` | warn | `ctx`, `err`, `status` |  |
| `preflight.route_unsure` | info | `choice`, `confidence`, `ctx`, `model` |  |
| `preflight.skills_capped` | warn | `asked`, `catalogued` |  |
| `preflight.skills_unknown` | warn | `ctx`, `skills` |  |
| `preflight.spawn_fail` | warn | `ctx`, `err` |  |
| `preflight.start` | info | `ctx`, `node` |  |
| `preflight.unsure` | info | `ctx`, `model`, `unsure` |  |
| `preflight.verdict` | info | `ctx`, `intent`, `needs_plan`, `risk`, `route`, `skills` |  |

### pressure

| Event | Level | Fields | Notes |
|---|---|---|---|
| `pressure.cleared` | info | `level`, `cause`, `disk_free_bytes` | Pressure is back to normal. |
| `pressure.shed` | warn | `level`, `cause`, `disk_free_bytes` | Below `min_free`, or memory pressure: admission stops, in-flight work drains. |
| `pressure.warn` | warn | `level`, `cause`, `disk_free_bytes` | Free space fell below twice `store.file.min_free`; low-priority work sheds. |

### proc

| Event | Level | Fields | Notes |
|---|---|---|---|
| `proc.exit` | error, info | `code`, `uptime_ms`, `turns`, `tool_calls`, `runs`, `tokens_in`, `tokens_out`, `err?` | The process is exiting with `code`. At error level with `err` when startup fails, or when an exit policy (`unavailable: exit`, `lifetime_exhausted: exit`) ends the instance. |
| `proc.ready` | info | `inbox_pending`, `instance`, `job_shape`, `runs`, `workflows` | Startup finished; triggers are armed. |
| `proc.start` | info | `config_files`, `instance`, `version` | The process started. |

### prompt

| Event | Level | Fields | Notes |
|---|---|---|---|
| `prompt.builtin.fail` | error | `note` |  |
| `prompt.failed` | error | `err`, `secret` | The `--prompt` task could not be delivered. |
| `prompt.reject` | warn | `err` |  |
| `prompt.render.fail` | error | `err`, `template` |  |
| `prompt.template.missing` | error | `err`, `template` |  |

### readback

| Event | Level | Fields | Notes |
|---|---|---|---|
| `readback.withheld` | info | `reader`, `reason`, `via` | A result that may carry outside input was withheld from a context holding `sensitive` and `egress`. |

### registry

| Event | Level | Fields | Notes |
|---|---|---|---|
| `registry.warning` | warn | `warning` |  |
| `registry.workflow_tools` | info | `arguments`, `mode`, `tags`, `tool`, `workflow` |  |

### restore

| Event | Level | Fields | Notes |
|---|---|---|---|
| `restore.adopted` | info | `artifacts`, `contexts`, `inbox_pending`, `lost`, `runs`, `subagents`, `timers` | Durable state was restored from the store at start. |
| `restore.context.lost` | warn | `ids` |  |
| `restore.done` | info | `generation`, `fresh_manifest`, `entities`, `lost`, `unindexed` |  |
| `restore.fresh` | info | `generation`, `superseded?`, `retired?`, `msg?` | A new generation started (first start, or `--fresh`). |
| `restore.run.corrupt` | warn | `err`, `id` |  |
| `restore.step.failed` | warn | `on_replay`, `run`, `step` |  |
| `restore.step.replay` | info | `attempt`, `run`, `step` |  |
| `restore.step.skipped` | info | `on_replay`, `run`, `step` |  |
| `restore.subagent.corrupt` | warn | `err`, `id` |  |
| `restore.task.corrupt` | warn | `err`, `id` |  |
| `restore.timer.lost` | error | `err`, `run`, `step`, `wait` | A timer could not be restored. |
| `restore.timer.repaired` | warn | `deadline_ms`, `run`, `step`, `timer`, `wait` |  |

### root

| Event | Level | Fields | Notes |
|---|---|---|---|
| `root.finish` | info | `ctx`, `exit`, `job_shape`, `status` | The root conversation called `finish`; `exit` says whether the process ends. |

### run

| Event | Level | Fields | Notes |
|---|---|---|---|
| `run.breakpoint` | info | `before_step`, `run` |  |
| `run.create.fail` | error | `err`, `workflow` |  |
| `run.deadline` | warn | `run` | A run passed its deadline; its children are cancelled. |
| `run.done` | info | `err`, `output`, `run`, `status`, `workflow` | A workflow run ended with `status`. |
| `run.dropped` | warn | `reason`, `workflow` | A start event was dropped by `concurrency.on_overflow: drop`. |
| `run.evict.fail` | warn | `err`, `run` |  |
| `run.evicted` | info | `run` |  |
| `run.inputs.invalid` | warn | `errors`, `workflow` |  |
| `run.paused` | info | `before_step`, `reason`, `run` | One run was paused. |
| `run.refused` | warn | `reason`, `run` | A run was refused with `reason`. |
| `run.resumed` | info | `run` | One run was resumed. |
| `run.stalled` | warn | `blocked_by`, `run` | A run can make no progress; `blocked_by` says why. |
| `run.start` | info | `acting_for`, `inbox_event`, `key`, `node`, `run`, `workflow` | A workflow run started. `acting_for` is the principal it runs for. |

### signal

| Event | Level | Fields | Notes |
|---|---|---|---|
| `signal.received` | info | `delivered`, `inbox_event`, `name` | A workflow signal arrived. |

### skill

| Event | Level | Fields | Notes |
|---|---|---|---|
| `skill.load.refused` | warn | `err`, `skill` |  |
| `skill.loaded` | info | `ctx`, `hash`, `skill` |  |
| `skill.unknown` | warn | `err`, `skill` |  |

### skills

| Event | Level | Fields | Notes |
|---|---|---|---|
| `skills.discovered` | info | `count`, `dir`, `server`, `skills` |  |
| `skills.file.unreadable` | warn | `err` |  |
| `skills.source.unavailable` | warn | `server` |  |

### start

| Event | Level | Fields | Notes |
|---|---|---|---|
| `start.a2a.fired` | info | `command`, `conversation`, `node`, `role`, `workflow` |  |
| `start.a2a.into` | info | `conversation`, `node`, `seq`, `stream`, `subject`, `workflow` |  |
| `start.a2a.into.refused` | warn | `err`, `node`, `stream`, `workflow` |  |
| `start.fired` | info | `workflow`, `node`, `kind`, stream fields | A start node fired. |
| `start.frozen` | warn | `workflow`, `node`, `kind`, `cause`, stream fields | A firing was refused because the instruction source is frozen. |
| `start.inbox.failed` | warn | `workflow`, `node`, `kind`, `err`, stream fields | A firing could not be written to the durable inbox. |
| `start.inputs.invalid` | warn | `workflow`, `node`, `kind`, `err`, stream fields | The start's `inputs` did not render; the firing is cancelled. |
| `start.key.empty` | warn | `node`, `template`, `workflow` |  |
| `start.key.invalid` | warn | `err`, `node`, `workflow` |  |
| `start.loop.stopped` | info | `node`, `reason`, `workflow` |  |
| `start.once.skipped` | info | `live`, `node`, `pending`, `workflow` |  |
| `start.schedule.armed` | info | `next_ms`, `node`, `workflow` | A `schedule` start is armed; `next_ms` is the next firing. |
| `start.schedule.done` | info | `node`, `note`, `workflow` |  |
| `start.schedule.invalid` | warn | `node`, `note`, `workflow` |  |
| `start.shed` | warn | `workflow`, `node`, `kind`, `cause`, stream fields | A firing was refused under resource pressure. |
| `start.subscribe.armed` | info | `workflow`, `node`, `server`, `uri`, `reason?` | A `subscribe` start is subscribed. |
| `start.subscribe.fail` | warn | `err`, `node`, `server`, `uri`, `workflow` | A subscribe failed and is retried on a backoff. |
| `start.subscribe.no_server` | warn | `node`, `server`, `workflow` |  |
| `start.subscribe.unsupported` | error | `err`, `node`, `server`, `uri`, `workflow` | The server does not support resource subscriptions; this start can never fire. |
| `start.too_large` | warn | `workflow`, `node`, `kind`, `err`, stream fields | The start event is larger than `store.max_value_bytes`. |
| `start.unknown_workflow` | warn | `inbox_event`, `workflow` |  |

### step

| Event | Level | Fields | Notes |
|---|---|---|---|
| `step.cache_hit` | info | `run`, `step` |  |
| `step.done` | info | `attempt`, `err`, `run`, `status`, `step`, `tokens` | A workflow step ended with `status`. |
| `step.goto` | info | `from`, `run`, `to` |  |
| `step.output.artifact` | info | `artifact`, `run`, `size`, `step` |  |
| `step.output.artifact_fail` | warn | `err`, `run`, `step` |  |
| `step.rate_wait` | info | `rate`, `run`, `step`, `wait_ms` |  |
| `step.rate_wait_fail` | warn | `err`, `run`, `step` |  |
| `step.retry` | info | `attempt`, `backoff_ms`, `run`, `step` | A step is retried after `backoff_ms`. |
| `step.start` | info | `attempt`, `kind`, `run`, `step` | A workflow step started. |
| `step.timeout_routed` | info | `run`, `step`, `to` |  |
| `step.turn.spawn` | info | `node`, `run`, `step` |  |

### store

| Event | Level | Fields | Notes |
|---|---|---|---|
| `store.checkpoint.fail` | error | `err` | A checkpoint failed. |
| `store.config_changed` | warn | `sections`, `msg` | State was written under a different configuration; resuming anyway. |
| `store.conflict` | warn | `key`, `seq`, `latest` | Another writer owns a key. Two processes share one instance identity. |
| `store.file` | info | `defaulted`, `generation`, `msg`, `path` | The file store's path and generation, and whether the path was defaulted. |
| `store.none` | warn | `note` | No durable store: state lives in this process only. |
| `store.put.fail` | warn | `key`, `err` |  |
| `store.seq_adopted` | info | `key`, `latest` |  |

### stream

| Event | Level | Fields | Notes |
|---|---|---|---|
| `stream.consumer.skipped` | warn | `events`, `from`, `node`, `stream`, `to`, `workflow` | Events a consumer had not read are gone (trimmed or unreadable). |
| `stream.emit` | info | `run`, `seq`, `step`, `stream`, `subject` |  |
| `stream.event.corrupt` | warn | `err`, `node`, `seq`, `stream`, `workflow` |  |
| `stream.forward.failed` | warn | `durable`, `err`, `peer`, `seq`, `status`, `stream`, `subject` |  |
| `stream.forward.refused` | warn | `err`, `run`, `step`, `stream` |  |
| `stream.head.recovered` | warn | `complete`, `from`, `stream`, `to` | An append found events past the recorded head and kept them. |
| `stream.read.failed` | warn | `stream`, `seq`, `workflow`, `node`, `err` | The store did not answer a consumer's read; the consumer holds and retries. |
| `stream.tap` | info | `audit_stream`, `include`, `queue`, `sampled`, `stream` | `observability.runtime_events` is armed. |
| `stream.tap.dropped` | warn | `events` | The runtime-events queue was full; this many were dropped. |
| `stream.tap.refused` | warn | `appended`, `events` |  |
| `stream.trimmed` | info | `events`, `first`, `stream` |  |

### subagent

| Event | Level | Fields | Notes |
|---|---|---|---|
| `subagent.cancel` | info | `reason` |  |
| `subagent.inject` | info | `bytes` |  |
| `subagent.kill` | info | `handle` | A subagent was stopped. |
| `subagent.respawn` | info | `handle`, `node` |  |
| `subagent.restore.bad_payload` | warn | `handle` |  |
| `subagent.result` | info | `err`, `handle`, `status`, `tokens` | A subagent reported its result. |
| `subagent.spawn` | info | `cpu_seconds`, `depth`, `handle`, `memory_bytes`, `mode`, `node`, `pid`, `priority`, `servers` | A subagent process started. |
| `subagent.swap_intel` | info | `endpoint_change` |  |
| `subagent.turn` | info | `handle`, `status` |  |

### timer

| Event | Level | Fields | Notes |
|---|---|---|---|
| `timer.unknown_owner` | warn | `id`, `owner` |  |

### tool

| Event | Level | Fields | Notes |
|---|---|---|---|
| `tool.call` | info | `id`, `tool`, `turn` | A turn called a tool. `args` only under `log_content`, in a subagent loop. |
| `tool.policy.ask` | info | `caller`, `rule`, `tool` | A policy rule asked a person before a tool call. |
| `tool.policy.decided` | info | `approved`, `by`, `rule`, `tool` |  |
| `tool.policy.error` | error | `caller`, `rule`, `tool` |  |
| `tool.policy.refused` | info | `action`, `caller`, `rule`, `tool` | A `security.policies` rule refused a tool call. |
| `tool.policy.unanswerable` | warn | `fallback`, `note`, `question`, `rule`, `tool` |  |
| `tool.reply` | debug | `is_error`, `node`, `req` |  |
| `tool.reply.dropped` | debug | `node`, `req` |  |
| `tool.request` | info | `args`, `caller`, `node`, `req`, `tool` | A child asked the supervisor to run an internal tool. `args` only under `log_content`. |
| `tool.result` | info | `is_error`, `ms`, `tool`, `turn` | A tool call returned. `content` only under `log_content`, in a subagent loop. |
| `tool.result.schema` | warn | `err`, `tool` |  |

### turn

| Event | Level | Fields | Notes |
|---|---|---|---|
| `turn.budget_wait` | info | `turn`, `wait_ms` |  |
| `turn.done` | info | `turn`, `status`, `rounds`, `tool_calls`, `tokens_in`, `tokens_out`, `kind?`, `node?`, `tokens?` | A turn ended. The child's line carries `tokens_in`/`tokens_out`; the supervisor's carries `tokens`. |
| `turn.effort_ignored` | warn | `agent_path`, `effort`, `model` |  |
| `turn.failed` | warn | `err`, `kind`, `node` |  |
| `turn.model` | info | `agent_path`, `model`, `provider`, `role` | The model a turn runs on, and the role that chose it. |
| `turn.model_degrade_failed` | warn | `err`, `to`, `turn` |  |
| `turn.model_degraded` | info | `from`, `provider`, `to`, `turn` |  |
| `turn.model_fallback` | info | `model`, `served_by`, `turn` | The model was unavailable and its fallback answered (or it is back). |
| `turn.reask` | info | `reason`, `turn` |  |
| `turn.refused.not_owner` | warn | `ctx`, `principal`, `reason` | A turn aimed at a conversation its principal does not own was refused. |
| `turn.reply` | info | `chars`, `ctx`, `deterministic`, `text` | A conversation turn replied. `text` only under `log_content`. |
| `turn.round` | debug | `round`, `tokens_in`, `tokens_out`, `tool_calls`, `turn` | One model round in a turn. |
| `turn.route_removed` | warn | `ctx`, `model`, `reason` |  |
| `turn.routed` | info | `ctx`, `model` | A turn was routed to a model by `intelligence.routes`. |
| `turn.spawn` | info | `ctx`, `inbox_event`, `node`, `turn` |  |
| `turn.spawn.fail` | error | `ctx`, `err` |  |
| `turn.start` | info | `kind`, `messages`, `tools`, `turn` | A turn began in a turn worker. |

### wait

| Event | Level | Fields | Notes |
|---|---|---|---|
| `wait.resolved` | info | `kind`, `run`, `status`, `step`, `stream`, `subject` |  |
| `wait.resubscribe.fail` | error, warn | `server`, `uri`, `reason`, `err`, `failed?`, `parked?` | A wait could not be subscribed again. Error level when it can never be woken. |
| `wait.resubscribed` | info | `reason`, `server`, `uri` | A suspended `wait on: resource` was subscribed again. |

### webhook

| Event | Level | Fields | Notes |
|---|---|---|---|
| `webhook.await.armed` | info | `path`, `run`, `step`, `url` |  |
| `webhook.await.restored` | info | `path`, `run`, `step` |  |
| `webhook.callback` | info | `path`, `resumed` |  |
| `webhook.denied` | warn | `kind`, `path`, `reason` | A webhook request failed authentication. |
| `webhook.duplicate` | info | `node`, `workflow` |  |
| `webhook.filtered` | info | `node`, `workflow` |  |
| `webhook.idempotency.expired` | info | `markers` |  |
| `webhook.idempotency.sweep.fail` | warn | `err` |  |
| `webhook.idempotency.unindexed` | warn | `err`, `node`, `workflow` |  |
| `webhook.idempotency.unrecorded` | warn | `err`, `node`, `workflow` | A delivery's idempotency marker could not be written; a replay runs again. |
| `webhook.into` | info | `node`, `seq`, `stream`, `subject`, `workflow` |  |
| `webhook.into.refused` | warn | `err`, `node`, `stream`, `workflow` |  |
| `webhook.overflow` | warn | `action`, `path` | A webhook route was at its `parallelism`; `action` says what was done. |
| `webhook.signal` | info | `node`, `resumed`, `signal`, `workflow` |  |
| `webhook.signal.invalid` | warn | `err`, `node`, `workflow` |  |

### webhooks

| Event | Level | Fields | Notes |
|---|---|---|---|
| `webhooks.listen` | info | `authority`, `bound`, `routes`, `tls` | The webhook listener bound. |
| `webhooks.routes` | info | `reason`, `routes` |  |

### workflow

| Event | Level | Fields | Notes |
|---|---|---|---|
| `workflow.defined` | info | `hash`, `name`, `op` |  |
| `workflow.deleted` | info | `name` |  |
| `workflow.durability_mix` | warn | `workflow`, `step`, `child`, `note` | A durable parent waits on a non-durable child. |
| `workflow.loaded` | info | `name`, `hash`, `starts`, `steps`, `durable` | A workflow definition went live. |
| `workflow.locked` | warn | `tool` |  |
| `workflow.pin_fail` | warn | `err`, `hash`, `workflow` |  |
| `workflow.pin_missing` | warn | `err`, `hash`, `workflow` |  |
| `workflow.pin_restored` | info | `hash`, `workflow` |  |
| `workflow.refused` | warn | `configured`, `name`, `tool` |  |
| `workflow.retiring` | info | `hash`, `live_runs`, `policy`, `reason`, `timeout_ms`, `workflow` | A workflow is being retired; its live runs drain under `policy`. |
| `workflow.stored.invalid` | warn | `name`, `errors`, `note?` | A definition the agent stored was not loaded; it is kept. |
| `workflow.stored.shadowed` | warn | — |  |
| `workflow.think.fail` | warn | `err`, `run` |  |
| `workflow.unloaded` | info | `hash`, `live_runs`, `reason`, `workflow` | A workflow was removed. |
| `workflow.unsubscribe_fail` | warn | `err`, `server`, `uri`, `workflow` |  |
| `workflow.unsubscribed` | info | `server`, `uri`, `workflow` |  |

## Metrics

`observability.metrics_addr` (`--metrics-addr`) serves Prometheus text format
0.0.4 at `/metrics`, beside `/healthz` and `/readyz`. It needs the `metrics`
build feature, which the release binaries and image include. The surface is
read-only and unauthenticated; `:9090` binds every IPv4 interface and logs
`metrics.bound_all_interfaces`.

Metric names and label keys are a stable contract: series are added, never
renamed, and a series is rendered with a value only where something writes
it. Labels carry bounded values only. A run id, agent path or URI never
appears in a label; label values outside a closed domain count under
`other`.

The **Status** column says what a scrape shows:

- **live**: written by the supervisor, the process that serves `/metrics`.
  Turn workers and subagents run in child processes with no `/metrics` of
  their own; what they count (model calls and their errors, what the last
  call found of its endpoint, loop steps) goes up the control channel with
  their next frame, and the supervisor adds it to its own.
- **marker**: only the `# HELP` and `# TYPE` lines, with no sample.

Use the log events for anything a series below does not cover.

### Liveness, readiness and lifecycle

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_up` | gauge | | live | `1` while the process answers the scrape |
| `agent_ready` | gauge | | live | `1` unless draining or every model endpoint is reported down; the same verdict as `/readyz` |
| `agent_paused` | gauge | | live | `1` while an instance-wide `admin.pause` holds |
| `agent_drains_total` | counter | `phase`: `started`, `completed`, `forced`, `other` | live | Drain transitions. An abandoned drain also counts `completed`; `forced` is never written. |
| `agent_config_reload_total` | counter | `result`: `applied`, `rejected`, `other` | live | Reloads: `applied`, or `rejected` when the reload was refused (`config.reload.invalid`, `config.reload.restart_required`) and the running configuration stands |
| `agent_config_generation` | gauge | | live | Applied reloads since start, the number also stamped into the durable manifest |
| `agent_reactor_stalls_total` | counter | | live | Times the supervisor loop went 5 s or more without a tick, which `/healthz` answers `503` for; counted when the loop ticks again. A slow start before the loop runs is not counted. |

### Runs, steps and turns

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_runs_started_total` | counter | | live | Workflow runs started: the arrival rate |
| `agent_runs_completed_total` | counter | | live | Runs that ended `completed` |
| `agent_runs_failed_total` | counter | | live | Runs that ended in any status but `completed` or `cancelled` |
| `agent_runs_killed_total` | counter | | live | Runs that ended `cancelled` |
| `agent_runs_total` | counter | `status`: `completed`, `failed`, `refused`, `cancelled`, `stalled`, `other` | live | Runs by terminal status, one count per run |
| `agent_runs_active` | gauge | | live | Workflow runs not yet terminal |
| `agent_steps_total` | counter | `status`: `done`, `failed`, `other` (timed out) | live | Workflow steps that ran, by how each attempt ended: the throughput signal. A retried step counts each attempt; a step that never ran (skipped, or on a branch not taken) is not counted. |
| `agent_turns_total` | counter | `kind`: `root`, `step`, `think`, `decide`, `preflight`, `compaction`, `other` | live | Turn workers started: `root` for a conversation turn, `step` for a workflow `agent`, `think` or `decide` step, `think` and `decide` for those tools, `preflight` and `compaction` for those thinks |
| `agent_turns_queued` | gauge | | live | Conversation turns waiting for a dispatch slot |
| `agent_inbox_pending` | gauge | | live | Events in the durable inbox not yet processed, A2A messages included: the backlog |
| `agent_reactions_total` | counter | | live | Start nodes fired (`start.fired`): a schedule, loop, subscription, webhook, A2A message, stream, `correlate` join, event or goal had its event accepted into the inbox. It counts when the event is accepted, so an event still waiting there behind a busy workflow is counted. A `once` start and a `workflow.run` call are not counted. |
| `agent_loop_steps_total` | counter | | live | Model rounds of turn workers and subagents, counted in the child and reported up |
| `agent_run_duration_ms` | histogram | | marker | Reserved; derive durations from `run.start` and `run.done` |

### Tokens and models

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_tokens_total` | counter | `type`: `in`, `out` | live | Tokens reported up by direct children |
| `agent_tokens_input_total` | counter | | live | Input tokens reported by direct children |
| `agent_tokens_output_total` | counter | | live | Output tokens reported by direct children |
| `agent_context_tokens` | gauge | | live | Estimated tokens of the largest live conversation context |
| `agent_budget_tokens_remaining` | gauge | | live | Tokens left under `intelligence.budget.lifetime_tokens`; absent without one |
| `agent_intel_calls_total` | counter | | live | Model calls: the supervisor's own (a decision, a judge) and those its children report |
| `agent_intel_up` | gauge | | live | `1` when the last model call, in whichever process made it, found its endpoint up and not every endpoint down |
| `agent_intel_errors_total` | counter | `reason`: `unreachable`, `auth`, `timeout`, `5xx`, `other` | live | Model calls whose sweep of the endpoints failed, by reason. A call that backs off and sweeps again counts each failed sweep. |
| `agent_intel_all_down` | gauge | | live | `1` while the last report from a child said every model endpoint is down: the condition `/readyz` and `agent_ready` read |
| `agent_intel_call_duration_ms` | histogram | | marker | Reserved |

### Store, pressure and memory

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_store_ops_total` | counter | `result`: `ok`, `conflict`, `error`, `other` | live | Durable store writes by result. A rising `conflict` means two processes share one instance identity. |
| `agent_store_latency_ms_sum` | counter | | live | Cumulative store write latency; divide by `agent_store_ops_total` for the mean |
| `agent_pressure_level` | gauge | | live | `0` ok, `1` warn, `2` shedding |
| `agent_disk_free_bytes` | gauge | | live | Free bytes on the file store's filesystem; absent without a file store |
| `agent_memory_max_bytes` | gauge | | live | cgroup v2 `memory.max`, when the kernel exposes it |
| `agent_memory_current_bytes` | gauge | | live | cgroup v2 `memory.current`, when the kernel exposes it |

### Subagents and children

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_subagents_spawned_total` | counter | | live | Child processes started: turn workers and subagents |
| `agent_subagents_exited_total` | counter | `status`: `completed`, `crashed`, `cancelled`, `other` | live | Child exits: `completed` (exit 0), `crashed` (another exit code), `cancelled` (a signal) |
| `agent_active_subagents` | gauge | | live | Subagents not yet ended, instance children included |
| `agent_tree_depth` | gauge | | live | Depth of the deepest of them: `1` for a subagent a turn or step spawned, `2` for one that subagent spawned |
| `agent_tree_breadth` | gauge | | live | The most of them that share a parent: a subagent, or the instance for those a turn or step spawned |
| `agent_subagent_restarts_total` | counter | `reason`: `restored`, `other` | live | Subagents spawned again: `restored` when a restarted supervisor finds one unfinished in its store (`subagent.respawn`, `instance.respawn`) |
| `agent_subagent_stuck_kills_total` | counter | `signal`: `kill`, `other` | live | Children the liveness check killed (`child.unhealthy`: stuck, or past their deadline), with `SIGKILL` |
| `agent_refusals_total` | counter | `reason`: `budget`, `policy`, `trifecta`, `depth`, `breadth`, `total`, `rate`, `other` | live | Refusals, by the guard that refused: `budget` a token budget refused a turn, a step or a model call; `policy` a `security.policies` rule refused a tool call (`tool.policy.refused`, `tool.policy.error`); `trifecta` a subagent's servers formed a lethal trifecta, or a read-back was withheld (`readback.withheld`); `depth`, `breadth`, `total` and `rate` a subagent spawn refused by that `limits.subagents` cap |
| `agent_limit_exceeded_total` | counter | `limit`: `steps`, `tokens`, `deadline`, `tokens_lifetime`, `other` | live | Work a bound stopped: a turn or subagent that ended `exhausted_steps`, `exhausted_tokens` or `deadline`, and the lifetime budget spent (once per process, with `budget.lifetime_exhausted`) |

### MCP, tools and reactive backlog

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_mcp_connect_failures_total` | counter | `server` (up to 15 names, then `other`) | live | Connect failures of the supervisor's MCP connections |
| `agent_tool_calls_total` | counter | | marker | Reserved; count `tool.result` lines |
| `agent_tool_call_duration_ms` | histogram | | marker | Reserved; use the `ms` field of `tool.result` |
| `agent_subscriptions_active` | gauge | `kind="resource"`; or `kind="event"` and `mode` | live | `kind="resource"`: the MCP resource subscriptions the connections hold (`subscribe` starts, resource waits, a resource instruction). `kind="event"`: active MCP Events bindings by `mode` (`poll`, `webhook`). |
| `agent_inflight_reactions` | gauge | | live | Work executing now: runs in `running`, not parked on a timer, gate or budget, however they started (a run from a `once` start or `workflow.run` counts too), and conversation turns in flight |
| `agent_reaction_lag_ms` | gauge | | live | Age of the oldest event waiting in the inbox; `0` when none waits. It rises while events queue behind a busy workflow. |

### Streams and MCP Events

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_stream_lag` | gauge | `stream`, `consumer` (`<workflow>/<node>`) | live | Events on a stream its consumer has not consumed. Up to 32 consumers per start kind by name; the rest fold into one `other` series carrying the largest lag. No sample when no consumer is armed. |
| `agent_mcp_events_total` | counter | `server`, `event`, `mode`, `outcome` | live | Occurrences by outcome: `appended`, `schema_mismatch`, `duplicate`, `rejected`, `refused_pressure`, `followed` (an event type only the instruction follows: taken, kept in no stream, and the instruction read again), and `unauthenticated` for a webhook |
| `agent_mcp_events_requests_total` | counter | `server`, `method`, `outcome` | live | `events/list`, `events/poll`, `events/subscribe` and `events/unsubscribe` requests: `ok`, `refused`, `failed` |
| `agent_mcp_event_gaps_total` | counter | `server`, `event`, `cause` | live | Gaps: `truncated`, `no_replay`, `schema_changed` |
| `agent_mcp_events_dedup_evicted_total` | counter | `server`, `event` | live | Dedup entries dropped to stay under the 4096 cap |
| `agent_mcp_event_lag_ms` | gauge | `server`, `event` | live | Append time minus the occurrence's server-reported timestamp, at least 0 |

The MCP Events families take `server` and `event` from the operator's
bindings (at most 256). A family past 512 series folds the rest into one
series whose labels all read `other`.

## Health surfaces

| Surface | Answers | Healthy | Unhealthy |
|---|---|---|---|
| `GET /healthz` | Is the supervisor's event loop making progress? | `200 ok tick_age_ms=<n>` | `503 unhealthy draining=<bool> tick_age_ms=<n>` when the last tick is 5 s old or a drain is under way |
| `GET /readyz` | Should new work be routed here? | `200 ready` | `503 not ready lame_duck=<bool> draining=<bool> intel_all_down=<bool>` |
| `GET /metrics` | The metrics above | `200`, `text/plain; version=0.0.4` | |

Any other path is `404`. The surface binds after startup finishes,
immediately before `proc.ready`, so a successful connection means the
runtime is up.
`intel_all_down` is the last report a child made: it turns true when a turn
found every model endpoint down and false when one answers again.

`observability.health_file` (`--health-file`) names a file the supervisor
rewrites once a second, atomically:

```json
{"alive":true,"draining":false,"mode":"1","run_id":"01M4P42Z69EJ2J6XDTWTFHP0GR","supervisor_tick_age_ms":128,"ts":"2026-10-11T18:37:32.731Z"}
```

| Field | Meaning |
|---|---|
| `ts` | When this record was written |
| `run_id` | The instance's run id |
| `mode` | Always `"1"`; carries no meaning |
| `supervisor_tick_age_ms` | Milliseconds since the supervisor's event loop last made progress |
| `alive` | `true` when the tick is younger than 5 s and no drain is under way |
| `draining` | `true` once a drain has begun |

The file is not removed when the process exits, so a reader must check that
`ts` is recent. A stale file with `"alive":true` means the writer stopped.

## OTLP export

The `otel` build feature exports over OTLP/HTTP with JSON bodies. The
collector is `observability.otel.endpoint`, else the
`OTEL_EXPORTER_OTLP_ENDPOINT` environment variable. agentd appends
`/v1/traces` or `/v1/logs` unless the URL already ends with it. Only a
plain `http://` collector is supported; an `https://` endpoint exports
nothing.

**Traces.** With a collector configured, each turn worker and subagent loop
exports one batch when it finishes: an `invoke_agent` span with a `chat`
child per model call and an `execute_tool` child per tool call. Spans share
the instance's `trace_id`. The resource carries `service.name: agentd` and
`service.version`.

| Span | Attributes |
|---|---|
| `invoke_agent` | `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| `chat` | `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| `execute_tool` | `gen_ai.operation.name`, `gen_ai.tool.name` |

A span's status is OK or ERROR. Export is best effort: a failure is dropped
and never fails the turn.

**Logs.** `observability.otel.logs: true` mirrors the supervisor process's
log lines to the collector every 5 s. Each record's body is the event name,
its severity the level, and the attribute `log.fields` the whole line as a
JSON string. At most 8192 records wait between flushes; the rest are dropped.

## Audit records

`observability.audit.sink` turns on the audit trail. A record is:

| Field | Meaning |
|---|---|
| `ts` | Milliseconds since the epoch |
| `instance` | The instance name |
| `principal` | Who acted: a principal id, `operator`, or `system` |
| `role` | The principal's role |
| `action` | What was done (below) |
| `target` | What it was done to |
| `outcome` | `ok`, or the refusal or failure |
| `request_id` | The A2A message id, when there is one |
| `trace` | The instance's `trace_id` |
| `sid` | The sign-in session, when the caller used one |

The `action` of an A2A call is `a2a.<method>`, or `a2a.<method>:<op>` when
the message carried a command: `a2a.SendMessage:admin.drain`,
`a2a.SendMessage:workflow.run`. A reload is `config.reload` with outcome
`applied`, `invalid` or `restart_required`. MCP Events runtime subscriptions
record `mcp.events.subscribed` and `mcp.events.unsubscribed`.

| Sink | Where the record goes |
|---|---|
| `log` | An `audit` line on stderr |
| `store` | A durable record in the configured store, never rewritten or listed |
| `stream` | An event on `observability.audit.stream`, subject `audit` |

A failed store write logs `audit.store.fail` and never fails the audited
action.

## Runtime events stream

`observability.runtime_events` copies selected event families, as the
supervisor process writes them, onto a declared stream, where `stream` start
nodes consume them. The subject is the event name and the data is the log
line.

| Key | Meaning |
|---|---|
| `stream` | The declared stream the events land on |
| `include` | Families taken in full |
| `sampled` | Families taken at 1 in 16 |
| `queue` | Events held between ticks before new ones are dropped and counted (default 512) |

Appends shed under resource pressure like any other admission, and a
refused append is dropped rather than retried (`stream.tap.dropped`,
`stream.tap.refused`). Lines written while the queue is being appended are
not captured, so the stream never feeds on its own retention trims.

## Next steps

- [Monitor in production](observability.md): how to use these signals.
- [CLI, environment and exit codes](cli.md): the exit-code contract.
- [A2A extensions](a2a-extensions.md): the observation feed's event kinds.
- [Configuration reference](configuration.md): every `observability.*` key.
