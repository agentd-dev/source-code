# @agentd-dev/cli

The display clients for [agentd](https://agentd.dev): a **terminal UI** and a
**web UI**, plus the thin-client core they share.

agentd hosts all state — conversations, tasks, workflow runs. These clients only
render it. Open the TUI at your desk and the web UI on another screen and both
show the same session, live, with no client-to-client protocol: each one
projects what the daemon says.

Both are ordinary **A2A 1.0 clients**. They read the agent's card, send
`A2A-Version: 1.0`, speak an agentd extension only when the card declares it,
sign in the way the card says, and fall back to core A2A when an extension is
absent — so they drive any A2A 1.0 agent, and agentd serves them nothing it
does not serve every client.

```sh
npm install -g @agentd-dev/cli

agentd tui -c agent.yaml                       # agentd and the terminal UI, signed in
agentd ui  -c agent.yaml                       # agentd and the web UI, opened signed in

agentd-tui --endpoint http://127.0.0.1:8420    # attach a terminal to a running agentd
agentd-ui  --endpoint http://127.0.0.1:8420 --open   # serve the web UI next to it
```

The live feed needs `a2a.events.enabled: true` in the daemon's configuration;
without it the clients poll with core A2A. See
[the interface guide](https://agentd.dev/docs/interface/) for everything below
in full.

## The extensions it speaks

A URI is used only when the card declares exactly that URI, and activated per
request with the `A2A-Extensions` header:

| URI | For |
|---|---|
| `https://agentd.dev/a2a/ext/command` | operations sent as one DataPart: `status`, `admin.set`, the auth ops, … |
| `https://agentd.dev/a2a/ext/events` | the observation feed, `agentd.events/SubscribeToEvents` |
| `https://agentd.dev/a2a/ext/task-annotations` | agentd's facts about a task: its link, principal, a gate's answer schema |
| `https://agentd.dev/a2a/binding/jsonrpc-unix` | a unix-socket interface — a binding this client cannot dial, so it passes such an interface over and names it when nothing else is left |

A card that marks any other extension `required` is refused.
`agentd-tui --no-extensions` (`?extensions=off` on the web UI) speaks core
A2A only.

## Connecting

`agentd-tui`:

| | |
|---|---|
| `--endpoint`, `-e <url>` | the agent: its base URL or its card's URL (or `AGENTD_ENDPOINT`) |
| `--bearer-file <path>` | a bearer token from a file (or `AGENTD_BEARER`, read and then removed from the environment) |
| `--login [--scope user\|operator]` | sign in with the OAuth device grant the card offers |
| `--no-extensions` | core A2A only |
| `--top`, `--bottom <items>` | the chrome layout (or `AGENTD_TUI_TOP` / `AGENTD_TUI_BOTTOM`; `/layout` lists the items) |
| `--debug` | open on the debug screen, while the daemon offers introspection |
| `--inline` | render into normal scrollback instead of fullscreen (or `AGENTD_TUI_INLINE=1`) |
| `--insecure` | skip TLS verification, for a self-signed development daemon (or `AGENTD_INSECURE=1`) |

`--launch-fd N` is how `agentd tui` hands the TUI its single-use sign-in; it is
not for hand use. Any other option exits 2 with `unknown option "<flag>" — see
--help`.

`agentd-ui [--endpoint <url>] [--port N | --listen-fd N] [--open]` serves the
web app on `127.0.0.1` (port 4173 by default). It holds no credential and reads
none from its environment or a URL: the page signs itself in, and keeps its
session in the tab's `sessionStorage` only. The daemon admits the page only
from an origin listed in `a2a.cors.origins` — except the page `agentd ui`
launches.

A TUI on a loopback daemon (http or https) with no credential configured
needs no credential. A browser always signs in: through the device grant, or as the tab
`agentd ui` opened (and, from that launcher's terminal, any tab after it).

## As a library

The package's entry point is the framework-free core both UIs are built on —
discovery, the JSON-RPC/SSE wire, the event-sourced `Mirror`, and the
`Observation` driver (card → bootstrap → events feed with cursor resume, or
core A2A polling when there is no feed):

```js
import { Mirror, Observation } from '@agentd-dev/cli';

const mirror = new Mirror();
const obs = new Observation({ configured: 'http://127.0.0.1:8420' }, mirror);
obs.start();                    // mirror.getState() is now a live projection
```

Build your own surface on that and it stays consistent with the shipped ones,
because they all fold the same state.

## Developing

```sh
npm install
npm run build       # the client core + TUI (tsc), then the web bundle (esbuild)
npm test            # unit + render tests
npm run typecheck
```

Node ≥ 20. Sources live in `src/{client,tui,ui}`. This package is **not** part
of the Rust workspace or its release artifact — agentd's own 3-dependency
default build is unaffected by anything here.

The protocol these clients speak is A2A 1.0 plus the extensions above, each
specified at its URI; the rendered overview is
[the interface guide](https://agentd.dev/docs/interface/).

AGPL-3.0-only — see
[LICENSE](https://github.com/agentd-dev/source-code/blob/main/LICENSE).
