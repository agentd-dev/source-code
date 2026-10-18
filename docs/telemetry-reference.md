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
| `trace_id` | yes | The W3C trace id of the unit of work the line is about: a run, a conversation turn, a start firing, a subagent, an A2A task, or else the process. A run another run started continues its trace, and so does work that arrived under a `traceparent`: an A2A request, a webhook, a stream event, an MCP Events occurrence. The process continues `observability.traceparent`, else mints a random one. |
| `span_id` | yes | The span the line is about: the run's, the step attempt's (`step.start`, `step.done`, `step.retry`), the worker's own, or an outbound hop's (`a2a.delegate`, `a2a.send`, `http.request`, `stream.emit`) — the span id that hop sent. With OTLP export on, it is the id of the exported span |
| `parent_span_id` | no | That span's parent, when it has one |

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
{"acting_for":"system","agent_id":"sup","agent_path":"0","comp":"supervisor","event":"run.start","inbox_event":"01M4P4004T51G3953PXYBDCPQS","key":null,"level":"info","node":"start","parent_span_id":"7c2d9e4f10a3b856","pid":1845135,"run":"main-01M4P4004VPT9M256ZXBGDGV3C","run_id":"01M4P4004MMJKM81HAZA0JFGP4","span_id":"3f8a6b1c0d2e4975","trace_id":"f281fdc06f4dd1374e53be619f96e1fe","ts":"2026-10-11T18:35:54.395Z","workflow":"main"}
{"agent_id":"run/main-01M4P4004VPT9M256ZXBGDGV3C/work","agent_path":"run/main-01M4P4004VPT9M256ZXBGDGV3C/work","comp":"agent","event":"turn.done","level":"info","parent_span_id":"5e3cf0a1d2b4c697","pid":1845136,"rounds":1,"run_id":"01M4P4004MMJKM81HAZA0JFGP4","span_id":"a1b2c3d4e5f60718","status":"completed","tokens_in":11,"tokens_out":5,"tool_calls":0,"trace_id":"f281fdc06f4dd1374e53be619f96e1fe","ts":"2026-10-11T18:35:54.412Z","turn":"main-01M4P4004VPT9M256ZXBGDGV3C/work#1"}
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
| `auth` | Display-client launch codes and sign-in sessions; outbound credentials that cannot be produced |
| `breaker` | Step circuit breakers |
| `budget` | Token budgets: windows, degrade, the lifetime cap |
| `cgroup` | cgroup v2 placement of children |
| `child` | Child process exits and health |
| `company` | Acting at Companies for a person (the `companies` feature): discovery, the client identity, Sessions and a Company's MCP servers |
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
| `otel` | OTLP export |
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
| `a2a.delegate` | info | `peer`, `run`, `step` | Its `span_id` is the delegation's, sent as `traceparent` on every request of it. |
| `a2a.denied` | warn | `principal`, `rule`, `method`, `op`, `reason`, `status`, `sid?`, `suppressed?` | The listener refused a request. `reason`: `unauthenticated`, `invalid_credential`, `no_role`, `not_permitted`, `rate_limited`, `auth_failures_limited`. Refusals a stranger can cause are written once per source and reason per minute; `suppressed` counts the rest. |
| `a2a.introspection` | info | `enabled` |  |
| `a2a.listen` | info | `authority`, `bound`, `device_grant`, `events`, `implicit_operator`, `introspection`, `mtls`, `required`, `tls`, `url` | The listener bound, with the posture it enforces. |
| `a2a.message.delivered` | info | `conversation`, `inbox_event`, `runs` | A message woke the waiting steps of `runs`. In the span the message was accepted in, which each run's `wait.resolved` names as `link`. |
| `a2a.push.failed` | warn | `err`, `task` | In the span the delivery sent. |
| `a2a.push.registered` | info | `config`, `principal`, `task` |  |
| `a2a.send` | info | `run`, `step`, `to` | Its `span_id` is the send's, sent as `traceparent`. |
| `a2a.task.continued` | info | `inbox_event`, `link`, `task` | A message continued a task. In the task's trace; `link` is the `traceparent` the request named. |
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
| `audit` | info | `instance`, `principal`, `role`, `action`, `target`, `outcome`, `request_id`, `trace`, `sid?`, `link?` | One audit record, when `observability.audit.sink` includes `log`. |
| `audit.emit` | info | `audit`, `run`, `step` | An `emit` step wrote an `audit` record. |
| `audit.store.fail` | warn | `action`, `err` | An audit record could not be stored. The audited action still stands. |

### auth

| Event | Level | Fields | Notes |
|---|---|---|---|
| `auth.launch.approved` | info | `client_id` |  |
| `auth.launch.exchanged` | info | `bind`, `client_id`, `sid`, `via` |  |
| `auth.launch.requested` | info | `client_id` |  |
| `auth.recovered` | info | `failures`, `target` | A request for `target` had its credential again after `failures` could not. Written at most once a minute, as `otel.export.recovered` is. |
| `auth.session.revoked` | info | — |  |
| `auth.unavailable` | warn | `err`, `failures`, `target` | The credential `target`'s `auth` (or `token_file`) presents could not be produced — no login, an expired token with no refresh token, an issuer, token endpoint or AWS source that does not answer, a secret file that cannot be read — so the requests that need it fail without being sent. Written when they start failing, then at most once a minute while they keep failing; `failures` counts the requests failed since the target's last `auth.recovered`. `target` is the credential's target: `intelligence`, `intelligence:<provider>`, `mcp:<name>`, `service:<entry>`, `a2a:<peer>` for a peer's own `auth`, or `aauth` for the AAuth identity — an Agent Provider that does not answer or whose metadata is not yet verified, one that will not enroll the key or issue an agent token, or a child whose key file no longer holds the daemon's key — which every request it signs needs. The supervisor writes it for the whole process tree: a turn worker or subagent hands its credential outcomes up, so a failure every turn meets is one line a minute. |

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

### company

Written by the supervisor, which alone acts at a Company. No event carries a
token, an assertion, a proof or a User ID; `session_id` appears only at
`debug`.

| Event | Level | Fields | Notes |
|---|---|---|---|
| `company.verified` | info | `company`, `domain`, `issuer`, `interfaces`, `servers`, `max_age` | The Company's discovery document and metadata verified; `servers` are its MCP servers by name. Written once per change. |
| `company.refused` | warn | `company`, `error`, `reason` | The Company's documents were not used. `reason`: `fetch`, `redirect`, `https`, `protocol_version`, `domain`, `auth`, `issuer`, `domains`, `es256`, `metadata`, `endpoint`, `mcp`, `mcp_names`. Written once per change; the Company is tried again with the backoff of a server down at boot. |
| `company.endpoint.refused` | warn | `company`, `field`, `host` | A URL the Company's documents name is off the entry's `hosts`: `field` names where it is. Beside `company.refused` with `reason: endpoint`. Also a device authorization's `verification_uri` or `verification_uri_complete` that is not an HTTPS URL with a plain host on the hosts, in printable ASCII: the sign-in is refused before a gate opens, and `host` is null when the URL names no plain host. |
| `company.discovery.stale` | warn | `age_s`, `company`, `error` | A refetch failed; the documents verified `age_s` seconds ago serve, for up to a day. |
| `company.issuer_changed` | warn | `company`, `new`, `old` | A refetch named another issuer: the Company is suspended until a restart. |
| `company.apis_changed` | warn | `added`, `company`, `removed` | A refetch lists other MCP APIs; no server changes until a restart. Written once per change. |
| `company.interface.refused` | warn | `company`, `error`, `interface`, `url` | An MCP server refused this agent outright (`extension_required`); it is not used again in this life. |
| `company.identity.unpublished` | error | `error`, `kid`, `url` | The client metadata document or key set at its URL is not what this configuration prints: every Company is unavailable until it is. |
| `company.seal_key.mismatch` | error | `error` | `security.companies.seal_key` opens none of the store's sealed User ID records, or is not set for a store whose records are sealed: every Company is unavailable, rather than meet a new User ID. |
| `company.session.started` | info | `company`, `signed_in` | A new Session at the Company. |
| `company.session.renewed` | debug | `company`, `signed_in` | The Session's Session Token renewed. |
| `company.session.ended` | info | `company`, `reason` | The Company ended the Session (`invalid_session`); a new one is started. |
| `company.session.id` | debug | `company`, `session_id` | The Session's id. |
| `company.privacy.standard` | warn | `company` | The entry sets `privacy: standard`. Written at boot. |
| `company.rate_limited` | info | `company`, `status`, `until` | The Company answered `429` or `503` with `Retry-After`: nothing is sent to it, by any task or interface, before `until` (RFC 3339; a wait over 5 minutes is cut to 5). Written when the instant moves later. |
| `company.clock_skew` | warn | `company`, `skew_s` | A response's `Date` is more than 30 s from agentd's clock (`skew_s` is the Company's minus agentd's). Proofs and assertions live about a minute, so the Company may refuse them. Written once per Company. |
| `company.cross_company.asked` | info | `answer`, `by`, `from`, `to`, `tool` | A call to the Company `to` from work holding the content of the Companies whose issuers are `from` asked the User, and ended with `answer`: `approve`, `deny`, `unanswered` (timed out or cancelled) or `unreachable` (nobody could be asked; the call was refused). `by` is who answered. Also an audit record, `company.gate`. |
| `company.browser.refused` | warn | `company`, `host`, `server` | A call on a declared browser (`security.companies.browsers`) named the Company's host `host` as a browser reads it, or a host that is not ASCII (`company` null): refused unsent. |
| `company.sign_in.requested` | info | `company`, `kind`, `scopes`, `task` | A sign-in gate opened on `task`: the User is asked to sign in by `kind` (`device`, `direct`) for `scopes`. Also an audit record, `company.sign_in` (`requested`). |
| `company.signed_in` | info | `company`, `granted`, `kind`, `requested` | The User signed in: the Account Token is kept sealed and the Session signed in. `granted` may be fewer than `requested`. Also an audit record, `company.sign_in` (`signed_in`). |
| `company.sign_in.failed` | info | `company`, `error`, `kind`, `reason` | A sign-in ended otherwise: `reason` `cancelled` (the User answered `cancel`, or the work that asked ended), `denied`, `expired`, `account_mismatch`, `interrupted` (agentd restarted while it was pending; also with `task`, or `run` and `step`) or `failed` (refused before a gate opened, or the Company's answer could not be used; `error` says why). Also an audit record, `company.sign_in`. |
| `company.sign_in.mixup` | warn | `company`, `iss` | A Direct Sign-In callback named an issuer other than the Company's (`iss`, quoted): its code is not exchanged, and the sign-in fails. |
| `company.sign_in.callback_refused` | warn | `reason`, `refused` | A Direct Sign-In callback whose `state` names no sign-in waiting — unknown, spent, or past its time — ends nothing. Said at most once a minute; `refused` counts them since agentd started. |
| `company.signed_out` | info | `by`, `company`, `error`, `revoked` | The Account Token was forgotten: by `company.sign_out` (`by` the principal) or `company.user_id.reset`. `revoked`: the Company revoked it (RFC 7009); otherwise `error` says why, and it stays good there until it expires. Also an audit record, `company.sign_out`. |
| `company.account_token.rotated` | info | `company` | The Company returned a new Account Token in a token response; it was kept sealed before the response was used. |
| `company.account_token.replaced` | info | `company`, `error`, `revoked` | A sign-in gave a new Account Token in place of the one held — more scopes, or another account — and the old one was revoked (`revoked`), or not (`error`). |
| `company.account_token.dropped` | info | `company`, `reason` | The Company answered `invalid_grant` for the Account Token — a disconnect, a revocation, an expiry: it is forgotten, and the Session goes on signed out. Nothing else forgets it. |
| `company.user_id.reset` | info | `company`, `started_over` | The operator reset the User ID the Company knows the User by (`company.user_id.reset`): the Company sees a new User from the next call on. `started_over`: the seal key opened none of the store's records, and this Company started over with it as a new User — the others stay unavailable until each is reset, and the store takes the key once all are. Its MCP connections join again on new sessions (`mcp.disconnect` `user_id_reset`). Also an audit record. |

`auth.unavailable` and `auth.recovered` carry `target: company:<name>`:
`auth.unavailable` while a Company cannot be verified or its Session cannot
produce a token, and `auth.recovered` once a token is produced again — a
Company that verifies while its token endpoint refuses has not recovered.

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
| `drain.interrupted` | info | `runs`, `steps`, `subagents`, `thinks`, `turns` | What the drain stopped and left for the next start: each step left `running` (`run/step`), each conversation turn with the inbox event that replays it (`ctx`, `inbox_event`), each think worker's purpose, each subagent recorded `parked`, and each `lifecycle.shutdown` run a forced drain cut short, which is cancelled rather than left. Written before `drain.done`, only when something was left. |
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
| `http.request` | info | `method`, `run`, `step`, `url` | An `http` workflow node made a request; its `span_id` is the one the request sent as `traceparent`. When the step's `headers` set `traceparent` or `tracestate`, agentd sent none and the line is in the step's span. |

### human

| Event | Level | Fields | Notes |
|---|---|---|---|
| `human.answer.not_addressed` | info | `addressee`, `from`, `link`, `task` |  |
| `human.answer.override` | warn | `addressee`, `by`, `link`, `sid`, `task` | An operator answered a gate addressed to someone else. |
| `human.answer.rejected` | info | `errors`, `link`, `task`, `via` |  |
| `human.answer.undelivered` | warn | `link`, `task`, `via` |  |
| `human.answered` | info | `by`, `labels`, `link`, `sid`, `task`, `via` | A person answered. In the gate's trace; `link` is the `traceparent` the answering request named, or null. |
| `human.ask` | info | `channel`, `deadline_ms`, `question`, `task` | A question went to a person. |
| `human.ask.drained` | info | `state`, `task` | The drain stopped the turn that asked. The gate's task is back to `TASK_STATE_WORKING`, and the turn's replay asks on it again; a gate task opened for that ask alone is `TASK_STATE_CANCELED`. |
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
| `instance.drain` | info | `handle`, `pid` | The parent's drain sent the child `SIGTERM`, so it drains itself. |
| `instance.exited` | info | `handle`, `pid`, `status`, `cause` | `cause`, for a child that failed, is the `err` its last `proc.exit` logged. `status` is `parked` for a child that drained at its parent's drain; the next start spawns it again. A `sync` child a stopped turn waited on is `cancelled` instead: the turn's replay spawns it again. |
| `instance.kill` | warn | `handle`, `note`, `pid` |  |
| `instance.metered` | info | `delta_tokens`, `handle`, `total_tokens` |  |
| `instance.mirror` | info | `handle`, `link`, `seq`, `stream` | `link`: the span of the child's report. |
| `instance.mirror.fail` | warn | `err`, `handle`, `stream` |  |
| `instance.op.fail` | warn | `err`, `op` |  |
| `instance.op.orphan` | warn | `handle`, `note`, `op` |  |
| `instance.op.refused` | warn | `op`, `principal`, `note` | A non-operator tried an instance operation. |
| `instance.op.unknown` | warn | `op` |  |
| `instance.parent_peer` | warn | `note` |  |
| `instance.respawn` | info | `handle`, `pid` |  |
| `instance.result` | info | `handle`, `link` | `link`: the span of the child's report. |
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
| `mcp.disconnect` | info, warn | `reason`, `server` | A connection closed. `reason`: `reload`, `reload refused`, `session_lost` (re-dialed), or `user_id_reset` (a Company's connection, dropped by `company.user_id.reset`; it joins again on a new one). |
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
| `otel.export.failed` | warn | `endpoint`, `err`, `failures`, `signal` | A batch of `signal` (`traces`, `logs`) did not reach the collector and was dropped. Written when exports start failing, then at most once a minute while they keep failing; `failures` counts the batches lost since the signal's last `otel.export.recovered`. |
| `otel.export.recovered` | info | `endpoint`, `failures`, `signal` | The collector took a batch after `failures` were lost. Written at most once a minute: after a failure inside that minute, which is logged at once, the next one waits for the first batch taken once the minute is up. |
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
| `restore.step.failed` | warn | `company?`, `on_replay?`, `reason?`, `run`, `step` | A step left running when the process stopped ended failed: its `on_replay` is `fail`, or (`reason: outcome_unknown`) a call of it to the Company `company` may have been sent, and is not sent again. |
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
| `run.dropped` | warn | `reason`, `run`, `workflow` | A start event was dropped by `concurrency.on_overflow: drop`. In the trace the run would have had; `run`: the id an answer already gave it (a webhook's `202`), or null. |
| `run.evict.fail` | warn | `err`, `run` |  |
| `run.evicted` | info | `run` |  |
| `run.inputs.invalid` | warn | `errors`, `run`, `workflow` | In the trace the run would have had; `run` as on `run.dropped`. |
| `run.paused` | info | `before_step`, `reason`, `run` | One run was paused. |
| `run.refused` | warn | `reason`, `run` | A run was refused with `reason`. |
| `run.resumed` | info | `run` | One run was resumed. |
| `run.stalled` | warn | `blocked_by`, `run` | A run can make no progress; `blocked_by` says why. |
| `run.start` | info | `acting_for`, `inbox_event`, `key`, `node`, `run`, `workflow` | A workflow run started. `acting_for` is the principal it runs for. |

### signal

| Event | Level | Fields | Notes |
|---|---|---|---|
| `signal.delivered` | info | `link`, `run`, `signal`, `step` | A signal woke a waiting step. In the waiting step's span; `link` is the signal's `traceparent` — a webhook callback's, an A2A caller's, the sending run's. |
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
| `start.a2a.into` | info | `conversation`, `links`, `node`, `seq`, `stream`, `subject`, `workflow` | In the appended event's own trace; `links` names the message's span. |
| `start.a2a.into.refused` | warn | `err`, `node`, `stream`, `workflow` |  |
| `start.fired` | info | `workflow`, `node`, `kind`, stream fields, `links?` | A start node fired. A `batch` or `correlate` firing starts a trace of its own, and `links` names each member event's `traceparent`. |
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
| `step.done` | info | `attempt`, `err`, `run`, `status`, `step`, `tokens` | A workflow step ended with `status`. In the span of the attempt, as `step.start` and `step.retry` are. |
| `step.goto` | info | `from`, `run`, `to` |  |
| `step.output.artifact` | info | `artifact`, `run`, `size`, `step` |  |
| `step.output.artifact_fail` | warn | `err`, `run`, `step` |  |
| `step.rate_wait` | info | `rate`, `run`, `step`, `wait_ms` |  |
| `step.rate_wait_fail` | warn | `err`, `run`, `step` |  |
| `step.retry` | info | `attempt`, `backoff_ms`, `run`, `step` | A step is retried after `backoff_ms`. |
| `step.retry.skipped` | info | `attempt`, `reason`, `run`, `step` | A failed step with a `retry:` is not retried: `reason: outcome unknown`, a Company call that failed after it was sent and may have been performed. |
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
| `stream.emit` | info | `run`, `seq`, `step`, `stream`, `subject` | Its `span_id` is the one the event records as `traceparent`. |
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
| `subagent.kill.escalated` | info | `handle`, `signal` | A killed subagent had not stopped at its grace, and its process group was sent `signal`: `SIGTERM` 5 s after the kill, `SIGKILL` 2 s later. |
| `subagent.respawn` | info | `handle`, `node` |  |
| `subagent.restore.bad_payload` | warn | `handle` |  |
| `subagent.result` | info | `err`, `handle`, `status`, `tokens` | A subagent reported its result. `cancelled` for a `sync` one a drain stopped with the turn waiting on it, which asks for it again when it replays, and for one `subagent.kill` stopped, with the kill's reason as `err`, however it ended. |
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
| `wait.resolved` | info | `conversation`, `kind`, `link`, `run`, `status`, `step`, `stream`, `subject` | In the waiting step's span: the run keeps its trace. For a `wait {on: event}`, `link` is the event's `traceparent`; for a `wait {on: message}` or `a2a.wait` (`kind: message`), the span the message was accepted in. |
| `wait.resubscribe.fail` | error, warn | `server`, `uri`, `reason`, `err`, `failed?`, `parked?` | A wait could not be subscribed again. Error level when it can never be woken. |
| `wait.resubscribed` | info | `reason`, `server`, `uri` | A suspended `wait on: resource` was subscribed again. |

### webhook

| Event | Level | Fields | Notes |
|---|---|---|---|
| `webhook.await.armed` | info | `path`, `run`, `step`, `url` |  |
| `webhook.await.restored` | info | `path`, `run`, `step` |  |
| `webhook.callback` | info | `link`, `path`, `resumed` | In the callback's own span, under the caller's; `link`: the `traceparent` the callback sent, or null. |
| `webhook.denied` | warn | `kind`, `path`, `reason` | A webhook request failed authentication. |
| `webhook.duplicate` | info | `node`, `workflow` |  |
| `webhook.filtered` | info | `node`, `workflow` |  |
| `webhook.idempotency.expired` | info | `markers` |  |
| `webhook.idempotency.sweep.fail` | warn | `err` |  |
| `webhook.idempotency.unindexed` | warn | `err`, `node`, `workflow` |  |
| `webhook.idempotency.unrecorded` | warn | `err`, `node`, `workflow` | A delivery's idempotency marker could not be written; a replay runs again. |
| `webhook.into` | info | `links`, `node`, `seq`, `stream`, `subject`, `workflow` | In the appended event's own trace; `links` names the request's `traceparent`. |
| `webhook.into.refused` | warn | `err`, `node`, `stream`, `workflow` |  |
| `webhook.overflow` | warn | `action`, `path` | A webhook route was at its `parallelism`; `action` says what was done. |
| `webhook.signal` | info | `link`, `node`, `resumed`, `signal`, `workflow` | In the span of the run or the event the delivery became. |
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
| `agent_drains_total` | counter | `phase`: `started`, `completed`, `forced`, `other` | live | Drains. `started` when one begins; at its end, `completed` when everything running finished within `drain_timeout`, or `forced` when the timeout, a second signal or an exit asked for stopped work that was still running, an abandoned drain included. |
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

### Companies

The supervisor's, with the `companies` feature. `company` is a company
entry's name; no label carries a User ID, a session or a conversation. A
family past 512 series folds as the MCP Events families do.

| Metric | Type | Labels | Status | Meaning |
|---|---|---|---|---|
| `agent_company_requests_total` | counter | `company`, `interface` (`mcp`), `outcome` | live | Calls to a Company's tools: `ok`; `error` (the Company's own answer: a tool's error or a refusal it sent); `unsent` (refused before sending: the not-before instant, the in-flight limit, a credential); `unknown` (failed after it was sent, and not sent again) |
| `agent_company_token_requests_total` | counter | `company`, `grant` (`start`, `renew`, `bearer`, `refresh`, `device_code`, `authorization_code`, `device_authorization`, `revoke`), `outcome` (`ok`, `refused`, `failed`) | live | Requests sent to the Company's token, device authorization and revocation endpoints, retries included |
| `agent_company_signed_in` | gauge | `company` | live | `1` while the Session there is signed in |
| `agent_company_rate_limited_until_seconds` | gauge | `company` | live | The Company's not-before instant, in Unix seconds; a past instant holds nothing back |
| `agent_company_gates_total` | counter | `company`, `kind` (`cross_company`, `sign_in`), `answer` (`approve`, `deny`, `unanswered`, `unreachable`; for `sign_in` how it ended: `signed_in`, `cancelled`, `denied`, `expired`, `account_mismatch`, `failed`) | live | The User's gates a Company call opened, by how each ended |

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
`/v1/traces` or `/v1/logs` unless the URL already ends with it. An
`observability.otel.endpoint` that is not an `http://` or `https://` URL,
such as a bare `host:port` or `grpc://`, is refused at load. An
`https://` collector is dialled over TLS and its certificate checked
against the bundled public roots plus `security.tls_ca`. A binary built
without the `tls` feature refuses an `https://`
`observability.otel.endpoint` at load; an `https://`
`OTEL_EXPORTER_OTLP_ENDPOINT`, read only at export, fails each export
instead.

**Traces.** With a collector configured, agentd exports one tree of spans
per trace. A span's id is the span id it has everywhere else: the
`span_id` of the lines about it, and the span a `traceparent` agentd sent
for it named. Its parent is the span it was started under, so a span a
downstream service exports for agentd's request is a child of an exported
span. What an MCP connection asks for itself — its handshake, its lists,
its subscriptions, an MCP Events poll — names the span of the process that
asks: `agentd`, or a worker's `invoke_agent`. The links of a span are the
`link` and `links` of the lines in it, at any log level, but for a link to
the span's own parent. A line naming them after its span has ended puts
them on a span of its own, named for the line's `event`, under that span.
A span in a trace that is not sampled — an upstream `traceparent` with
flags `00` — is not exported. The resource carries `service.name: agentd`
and `service.version`.

The supervisor exports its spans from a background thread every 2 s, and
what it still holds when it exits. A turn worker or a subagent exports a
turn's spans when the turn ends. An exit waits at most 3 s on these last
exports, spans and log records together: once a batch fails or the time
is up, the rest is dropped and counted lost.

| Span | Kind | Exported | Parent | Attributes |
|---|---|---|---|---|
| `agentd` | INTERNAL | at exit | `observability.traceparent`'s span, if any | `agentd.instance`, `agentd.run_id`, `agentd.exit_code` |
| `run <workflow>` | SERVER for a run a `workflow.run` over A2A or a webhook delivery started, CONSUMER for a `stream` or `correlate` start's, else INTERNAL (an `a2a` start's included) | when the run ends | the span its start continued: the caller's, the message's for an `a2a` start, the emit's, the starting step's, the process's in a job | `agentd.run`, `agentd.workflow`, `agentd.start`, `agentd.task`, `agentd.run.status` |
| `step <step>` | INTERNAL | when the attempt ends | the run's | `agentd.run`, `agentd.step`, `agentd.step.kind`, `agentd.step.attempt`, `agentd.step.status` |
| `SendMessage` | SERVER | when the message is routed to a turn, a run or a waiting step | the caller's | `a2a.task.id`, `agentd.conversation`, `agentd.message.routed` |
| `message` | INTERNAL | the same, for a message the runtime delivered itself: the `--prompt`, a `message` step | the sender's | as `SendMessage` |
| `<A2A method>` | SERVER | when answered: a command answered at once, and any other A2A request that started no task under the caller's span, when its `traceparent` named one | the caller's | `rpc.system`, `rpc.method`, `a2a.task.id`, `agentd.command` |
| `<METHOD> <route>` | SERVER | when answered: a webhook delivery that started no run under the sender's span, when its `traceparent` named one. Linked to the event it appended or the run a duplicate names. A callback is `<METHOD> webhook callback`, the span its `webhook.callback` line is in | the sender's | `http.request.method`, `http.route`, `http.response.status_code` |
| `signal <name>` | INTERNAL | when delivered | the sender's | `agentd.signal`, `agentd.signal.delivered` |
| `emit <stream>` | PRODUCER | when appended | the step's | `messaging.operation.type`, `messaging.destination.name`, `agentd.stream.seq` |
| `append <stream>` | PRODUCER | when an `into:` appends: a trace of its own, linked to the request's span | none | as `emit` |
| `tools/call <tool>`, `resources/read <uri>`, `prompts/get <name>`, `completion/complete <server>`, `resources/list <server>`, `resources/templates/list <server>` | CLIENT | when answered | the step's, the turn's, or the tool call's | `mcp.method.name`, `agentd.mcp.server`, `gen_ai.operation.name` and `gen_ai.tool.name` (`tools/call`), `mcp.resource.uri` (`resources/read`), `gen_ai.prompt.name` (`prompts/get`) |
| `<METHOD>` | CLIENT | when answered: an `http` step's request, or a `forward: {webhook}` push | the step's, or the emit's | `http.request.method`, `http.response.status_code`; an `http` step's `url.full`, `agentd.run`, `agentd.step`; a forward's `url.scheme`, `server.address`, `server.port`, `messaging.destination.name` — not its URL, which can be a credential |
| `a2a.send <peer>`, `a2a.delegate <peer>`, `instance.send <handle>`, `forward <stream>` | CLIENT | when answered; a delegation is one span, its card read, send and polls | the step's, the turn's, or the emit's | `rpc.system`, `rpc.method`, `agentd.a2a.peer`, `agentd.instance.handle`, `messaging.destination.name` |
| `a2a.push` | CLIENT | when delivered | the task's | `http.request.method`, `url.scheme`, `server.address`, `server.port` — not the push URL, which can be a credential |
| `exec <command>` | INTERNAL | when the command exits: the span its `TRACEPARENT` named | the step's, or the turn's that called the tool | `process.executable.name`, `process.exit.code` |
| `<event>` | INTERNAL | when a line names `link` or `links` after the span it is in has ended: a `human.*` line about a conversation turn's gate, an `instance.*` line after the spawning step | the line's span | the line's links |
| `chat` | CLIENT | when answered: a judge's or the goal check's model call | the gate's or the unit's | `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |

| Span (a turn worker's or a subagent's) | Kind | Parent | Attributes |
|---|---|---|---|
| `invoke_agent` | INTERNAL | the step's, or the message's for a conversation turn; a subagent's, the span that spawned it | `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| `chat` | CLIENT: one per model call, the span its request sent, kept by its retries, failovers and fallback | `invoke_agent` | `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens` |
| `execute_tool` | CLIENT for an MCP tool, the span its request sent; INTERNAL otherwise | `invoke_agent` | `gen_ai.operation.name`, `gen_ai.tool.name`, `mcp.method.name`, `agentd.mcp.server` |

A decide turn is an `invoke_agent` span with a `chat` span per decision
call. A span's status is OK or ERROR. Export is best effort and never fails
the work: each step of a send (connect, write, read) gives up after 5 s, at
most 8192 spans wait in a process, and a batch that fails is dropped, not
retried. An exit gives its last exports 3 s in all.

**Failures.** A failed export is logged as `otel.export.failed` (warn),
naming the signal (`traces` or `logs`), the endpoint and the error, when
exports start failing, then at most once a minute while they keep failing.
The first batch the collector takes after that is logged as
`otel.export.recovered` (info), also at most once a minute: if exports fail
again within a minute of that line, the failure is logged at once, and the
next `otel.export.recovered` waits for the first batch taken after that
minute. So a collector that drops every other batch gets one line of each
a minute. `failures` counts the batches lost since the signal's last
`otel.export.recovered`, those an exit dropped behind a failed batch
included. Each process counts on its own: a turn worker
exports one trace, so each turn whose trace fails logs one line, while the
supervisor's span and log exports keep one count each for as long as it
runs.

**Logs.** `observability.otel.logs: true` mirrors the supervisor process's
log lines to the collector every 5 s, and what is left when it exits. Each
record's body is the event name, its severity the level, its `traceId` and
`spanId` the line's `trace_id` and `span_id`, and the attribute `log.fields`
the whole line as a JSON string. At most 8192 records wait between flushes;
the rest are dropped.

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
| `trace` | The `trace_id` of the unit of work the action belongs to (a run, a turn, a tool call's, an A2A request's task), else the instance's |
| `sid` | The sign-in session, when the caller used one |
| `link` | The `traceparent` of a request from another trace that caused the action without starting it — the answer to a gate, a message continuing a task, any A2A call on a task of another trace — when there is one |

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
