# The interface — TUI & web UI

agentd ships two **display clients** — a terminal UI and a web UI — built as
separate Node projects under [`interface/`](../interface). They are *thin* by
design (RFC 0032): **agentd hosts all state, tools and secrets; the clients
only render daemon state and forward your intent.** Open both at once — plus a
colleague's browser — and every surface shows the same conversation, tasks and
runs, live, because each one watches the same daemon feed. None of them holds
any truth of its own.

```
             ┌────────────┐   SubscribeToEvents (SSE feed)   ┌───────────┐
  agentd ────┤ A2A listener├──────────────────────────────────┤ agentd-tui│
  (state,    │ (a2a.listen)│◄────── SendMessage / Cancel ─────┤ agentd-ui │
   tools,    └────────────┘                                   │ browser…  │
   secrets)        one daemon, N synchronized displays        └───────────┘
```

## 1. Enable it

The interface is **off by default**. Turn it on in the config:

```yaml
a2a:
  listen: http://127.0.0.1:8420     # the interface rides the A2A listener
interface:
  enabled: true                     # serve the display-client surface
  debug: false                      # extra information (see §5)
```

With `enabled: false` the daemon's wire surface is byte-identical to a build
without the feature — clients get a clear "interface is disabled" error.

Auth is the A2A listener's (RFC 0029): on a plaintext loopback listener with no
principals a local client is the **operator** with zero setup; a remote client
presents `a2a.bearer` / an mTLS identity and sees only what its role and
ownership allow.

## 2. One command: `agentd tui` / `agentd ui`

The passthrough runs the daemon **and** its display client together:

```sh
agentd tui --config code.yaml            # daemon + terminal UI
agentd ui  --config code.yaml            # daemon + web UI (opens the browser)
agentd tui --config code.yaml --debug    # …with the debug surface on
```

The subcommand forces `interface.enabled` on, redirects the daemon's log lines
to a file (the path is printed first; `AGENTD_INTERFACE_LOG` overrides), hands
the terminal to the client, and ties the lifetimes: quitting the client drains
the daemon gracefully; the daemon exiting closes the client. The client binary
is found on PATH (`npm install -g @agentd/tui` / `@agentd/ui`;
`AGENTD_TUI_BIN` / `AGENTD_UI_BIN` override).

## 3. Detached: connect to any running agentd

Run the daemon on its own and attach displays whenever you like:

```sh
agentd --config code.yaml                       # the daemon
agentd-tui --endpoint http://127.0.0.1:8420     # a terminal, any time
agentd-ui  --endpoint http://127.0.0.1:8420 --open   # a local web UI
```

- `agentd-tui` flags: `--endpoint` (or `AGENTD_ENDPOINT`), `--bearer` (or
  `AGENTD_BEARER`), `--debug` (open on the debug screen), `--insecure`
  (self-signed dev TLS).
- `agentd-ui` serves the built web app on `127.0.0.1:4173` (`--port`) with the
  endpoint pre-filled; `--open` launches the browser. The page also takes
  `?endpoint=…` and remembers your last connection.
- **Hosted web UI:** `interface/ui/dist/` is a static site — deploy it
  anywhere, then allow its origin on each daemon it should reach:

  ```yaml
  interface:
    enabled: true
    origins: ["https://ui.example.com"]
  ```

  Loopback origins (an `agentd-ui` on the same machine) never need listing.
  Any other cross-site origin remains rejected (the DNS-rebinding guard).
- Connecting to a **remote** daemon is the same `--endpoint https://…` plus its
  bearer; everything a client can see or do is decided by the daemon's
  principal rules, not by the client.

## 4. What you can do

Both clients speak the same surface:

- **Chat** — natural-language turns with the agent. The transcript shows every
  attached client's prompts (labelled by principal when not you), command
  invocations, and the agent's replies; an `input-required` task renders as an
  answerable row, and a plain reply answers it. `Esc` cancels the newest
  working task.
- **The composer speaks four prefixes** (suggestions appear as you type; Tab
  accepts):
  - `/` — commands: `/help /new /tasks /subagents /debug /status
    /config [path] /set <path> <value> /workflow <name> /cancel [task] /pair
    /drain /quit` — **plus every workflow as a shortcut** (`/deploy` runs the
    `deploy` workflow; system names win).
  - `@` — **skills**: `@release-notes` autocompletes from the daemon's
    catalogue and stays in the text (agentd preloads referenced skills).
  - `#` — **targets**: start a message with `#task-…` to answer/continue that
    task (the way to answer a specific input-required question), or `#<ctx>`
    to address that conversation. Inline `#…` is plain text.
  - `$` — **live values**: `$model $instance $version $turns $tokens $tasks`
    interpolate daemon state into your message; `$$` escapes a dollar.
- **Tasks** — every task your principal may see, live states, cancel.
- **Subagents** — the live list (handle · mode · status · tokens); select/click
  one for the detail view (instruction, result, attempts, errors — needs
  debug) and step back to the list. TUI: `↑/↓` + Enter, `Esc` back.
- The status bar shows the connection (`● live` = feed; `◐ polling` = a daemon
  without the feed — the clients degrade to polling automatically), counters,
  and a prominent DRAINING notice.

### 4.1 Configure the chrome (`interface.display`)

The **daemon** decides what its clients render in the top (header) and bottom
(status bar) edges — every attached surface lays out the same:

```yaml
interface:
  display:
    top: [name, model, instance, debug]
    bottom: [conn, tokens, turns, runs, subagents, clock]
```

Items: `name` `version` `instance` `model` `endpoint` `conn` `debug`
`draining` `active` `turns` `tokens` `tool_calls` `runs` `subagents`
`conversations` `screen` `keys` `clock`. Unknown items are skipped (a warning
at config validation); `screen`/`keys` are TUI-only. Omit `display` for the
defaults. The layout is also **runtime-shapeable**:
`/set interface.display.bottom ["conn","model","tokens"]` re-shapes every
connected client at once.

### 4.2 Runtime settings (`/set`) — and their deliberate limit

`config.set` (operator) updates a whitelisted set of knobs in the running
daemon, no restart:

- `/set interface.debug true` — open up the debug surface live (and back off);
- `/set interface.display.top …` / `…bottom …` — reshape the chrome (§4.1).

Everything else answers with the whitelist and stays where it belongs: the
config file + SIGHUP hot reload (see configuration.md §11) — the daemon never
mutates config it doesn't own. `/config` prints the full effective document,
`/config a2a.listen` one value.

### 4.3 Pairing-code login (`interface.pairing`)

The no-copy way to connect a browser or a remote TUI:

```yaml
interface:
  enabled: true
  pairing:
    enabled: true      # default off
    role: operator     # what a paired client becomes (operator | user)
    ttl: 12h           # session lifetime
```

Flow: the **operator** runs `/pair` in their TUI (or web UI) — it prints a
**6-digit code that rotates every minute** plus a ready-made connect command.
The joiner enters the code — `agentd-tui --endpoint … --code 483921`, or the
code field on the web connect form — and receives a **session token** used
automatically from then on. The code is only a bootstrap: verification is
constant-time and rate-limited (5 misses per minute lock it out), the minted
token is 32 bytes of OS randomness, and sessions live in memory — restarting
the daemon revokes everything. On a non-loopback listener, pairing counts as
client auth (you can run TLS + pairing with no static bearer at all).

## 5. Debug mode — the extra information

```yaml
interface:
  enabled: true
  debug: true        # or: agentd tui -c … --debug
```

`debug` is a **daemon-side** switch: clients learn it from `interface.info`
and only then render their debug screens. It unlocks:

- the **feed tail** — every observation event as it happens (tasks, messages,
  runs, subagents, audit records);
- **runs** with per-step detail (`run.get`: status, attempts, timings, errors,
  waits, outputs);
- **conversation transcripts** with message bodies (`conversation.get`) —
  the one read that exposes content, which is why it rides this gate;
- the **live log ring** (`debug.events`) — the daemon's own JSON-lines
  telemetry, tailed in the client.

Treat `debug: true` as operator-grade exposure; leave it off in production
unless you need it.

## 6. The protocol (for other clients)

Everything above is plain A2A JSON-RPC (RFC 0029) plus the RFC 0032 additions —
any program can be a display client:

- `SubscribeToEvents {fromSeq}` — the SSE feed: `hello` → `event`* → `goodbye`;
  reconnect with the goodbye cursor; `hello.resync` means re-bootstrap via the
  `status` command. Events are principal-scoped.
- Taskless reads (command DataParts): `interface.info`, and under debug
  `conversation.get`, `run.get`, `debug.events`.
- The reply to any prompt arrives as its task's terminal artifact on the feed —
  the same event every other client folds in.

The reference implementation is the shared TypeScript core
[`@agentd/client`](../interface/client) (wire + state mirror + observation
driver with poll fallback); both shipped UIs are ~thin renderers over it.

## 7. Building the clients

```sh
cd interface
npm install
npm run build          # client → tui → ui
npm test               # unit + render tests
```

Node ≥ 20. The clients are **not** part of the Rust workspace or its release
artifact; the daemon's 3-dependency default build is unchanged.
