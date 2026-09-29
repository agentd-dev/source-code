# The events extension

| | |
|---|---|
| URI | `https://agentd.dev/a2a/ext/events` |
| Schema | [`https://agentd.dev/a2a/ext/events/schema.json`](https://agentd.dev/a2a/ext/events/schema.json) (JSON Schema draft 2020-12) |
| Kind | A2A method extension: it adds the method `agentd.events/SubscribeToEvents` |
| Required | never |
| Declared | while `a2a.events.enabled` is on (restart-only) |
| Applies to | `agentd.events/SubscribeToEvents` |

This page is the normative specification of the URI above. It is served at
that URI.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST, and a version in a name agentd owns would be
a second name for the same thing. An incompatible change takes a new URI with a
new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's meaning MUST NOT
change under a peer that already speaks it.

A2A streams one task at a time (`SubscribeToTask`). A display client needs the
whole instance: every run, conversation and subagent it may see, as it changes.
The protocol has no such method, so this extension declares one. The method is
namespaced under the extension's name, so a method the specification defines
later can never collide with it.

## Params

| key | type | card | meaning |
|---|---|---|---|
| `method` | string | public | `agentd.events/SubscribeToEvents` |
| `schema` | string | public | the URL of this extension's schema bundle |
| `ring` | integer | extended | how many events the feed keeps for replay: how far behind a reconnecting subscriber may be and still resume |
| `kinds` | string[] | extended | the kinds this caller's subscription can carry (see [Kinds](#kinds)) |

## Activation

The method is part of the extension. A request that calls it is answered only
when the instance declares the extension and the request's `A2A-Extensions`
header names the URI. Otherwise the answer is `-32601`, with the reason
`EXTENSION_NOT_DECLARED` or `EXTENSION_NOT_ACTIVATED` and the URI in its
metadata. The response echoes the URI when it was activated.

## The method

```json
{"jsonrpc": "2.0", "id": 7, "method": "agentd.events/SubscribeToEvents", "params": {"fromSeq": 0}}
```

`params` is `$defs/params`: at most `fromSeq`, a non-negative integer cursor.
Absent params and an absent `fromSeq` mean the start of the feed. Any other
member is refused with `-32602` and the member named, because ignoring a
misspelt cursor would silently replay from the start.

The answer is a `text/event-stream`. Each SSE event's `data` is one JSON-RPC
response carrying the request's `id`, and its `result` is one frame. The bundle
validates a frame at its root. A frame is exactly one of the following.

- **`hello`**: the first frame. `{seq, resume, resync, introspection, version}`
  - `seq` is the newest sequence number in the feed.
  - `resume` is the cursor the client asked for.
  - `resync` is `true` when that cursor can no longer be honoured: events past
    it were evicted from the ring, or it is ahead of the feed, as every
    client's cursor is after a daemon restart, because the feed lives in memory.
    The stream then replays from the start of the ring, and the client
    should re-bootstrap its state instead of applying the replay on top of it.
  - `introspection` is whether `a2a.introspection.enabled` is on.
  - `version` is the agentd build. It is not a protocol version: the
    protocol is the URI.
- **`event`**: `{seq, ts, kind, data}`
  - `seq` increases along the feed.
  - `ts` is in epoch milliseconds.
  - `kind` is one of the [kinds](#kinds), and `data` is that kind's
    document, `$defs/kinds/$defs/<kind>` in the bundle.
  - The events past the cursor are replayed first, then live events follow.
  - A subscriber sees only the events it may see, so the `seq` values it reads
    have gaps.
- **`goodbye`**: the last frame. `{seq, reason}`
  - `seq` is the cursor to resume from with `fromSeq`, so a reconnect is a
    continuation.
  - `reason` is `deadline` when the listener's stream deadline elapsed.
  - `reason` is `revoked` when the caller's session was revoked. No event
    past the revocation is sent.

## Kinds

The closed vocabulary. The feed never pushes a kind outside it: debug builds
assert every push against its kind's schema, and a source scan holds every push
site in the tree to this list. The "who receives it" column is the widest
audience. A subscriber is sent an event only when that event's visibility
admits the subscriber.

| kind | who receives it | what it says |
|---|---|---|
| `task` | its owner and operators | a task changed: the A2A `Task`, its `history` cut to the newest few messages |
| `task.removed` | its owner and operators | a task left the task store (retention); `{id}` only |
| `run` | its owner and operators | a workflow run's summary: status, per-step states, tokens, output, error |
| `run.removed` | its owner and operators | a run left the status view; `{id}` only |
| `step` | operators | a step started (`phase: start`) or finished (`phase: done`) |
| `conversation` | its owner and operators | a conversation's status: size, turns, plan, skills |
| `conversation.removed` | its owner and operators | a conversation left the status view; `{id}` only |
| `subagent` | operators | a subagent's status |
| `subagent.removed` | operators | a subagent left the status view; `{id}` only |
| `child` | operators | an OS child process of the instance |
| `child.removed` | operators | a child process left the status view; `{id}` only |
| `activity` | its owner and operators | live activity of a unit of work: phase, tool, round, tokens |
| `activity.removed` | its owner and operators | the unit of work left the status view; `{id}` only |
| `status` | operators | the instance's slim status: draining, inbox, counters, budget, store |
| `lifecycle` | every subscriber | the instance began draining, or was paused or resumed as a whole |
| `config` | every subscriber | settings moved: which paths, and whether a reload or `admin.set` moved them |
| `audit` | operators, while `a2a.introspection.enabled` | the audit mirror: who did what to what, and the outcome |
| `auth` | operators | a device sign-in pending, approved or denied; a session revoked; a launch |

A departure (`*.removed`) names what left and nothing else: its owner already
holds the rest, and anyone else must not learn it.

## Security

- **Named callers only.** An anonymous caller is refused before the method
  runs, like every other method.
- **Visibility is per event.** Every event is pushed with a visibility tag,
  checked per subscriber on every read of the ring. An operator is sent every
  event. A named caller is sent the events its principal owns and those that
  reach every subscriber. The same rule answers the `status` read.
- **Annotations follow activation.** A `task` event's `Task` carries the
  [task-annotations](task-annotations.md) only when the subscribing request also
  activated that extension. The ring holds each event once, and the
  annotations are removed per subscriber.
- **Revocation ends the stream.** A subscriber whose sign-in session is revoked
  is sent `goodbye{reason: "revoked"}` within one tick, and nothing after the
  revocation.

## Dependencies

None required. A display client typically bootstraps its state with the
[command extension](command.md)'s `status` read, then applies events on top.
It reads `status` again when `hello.resync` is true.

## Schema and examples

The bundle at [`schema.json`](https://agentd.dev/a2a/ext/events/schema.json)
validates a frame, the `result` of each SSE event, at its root:

- `$defs/params` holds the method's params;
- `$defs/hello`, `$defs/event` and `$defs/goodbye` hold the three frames;
- `$defs/kinds/$defs/<kind>` holds each kind's `data`.

`agentd --extension-schema events` prints the bundle. It is generated from the
table the feed's push is checked against, and CI fails when the published copy
differs from it. Golden frames, each valid against the bundle, are published
beside it under
[`examples/`](https://github.com/agentd-dev/source-code/tree/main/web/public/a2a/ext/events/examples).
