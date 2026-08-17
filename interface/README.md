# @agentd-dev/cli

The display clients for [agentd](https://agentd.dev): a **terminal UI** and a
**web UI**, plus the thin-client core they share.

agentd hosts all state — conversations, tasks, workflow runs. These clients only
render it. Open the TUI at your desk and the web UI on another screen and both
show the same session, live, with no client-to-client protocol: each one
projects the daemon's event feed.

```sh
npm install -g @agentd-dev/cli

agentd-tui --endpoint http://127.0.0.1:8420    # terminal UI (fullscreen)
agentd-ui  --endpoint http://127.0.0.1:8420    # web UI, served locally
```

Or let the daemon start one for you — `agentd tui -c agent.yaml` runs both and
ties their lifetimes.

The daemon must have the interface surface enabled:

```yaml
interface:
  enabled: true      # default OFF
a2a:
  listen: http://127.0.0.1:8420
```

## Connecting

| | |
|---|---|
| `--endpoint <url>` | the daemon's A2A listener (or `AGENTD_ENDPOINT`) |
| `--bearer <token>` | when the daemon requires one (or `AGENTD_BEARER`) |
| `--code <123456>` | pair with the rotating code an operator reads from `/pair` |
| `--debug` | ask for debug panes (the daemon decides whether to serve them) |
| `--inline` | TUI: render into normal scrollback instead of fullscreen |

A loopback client is the operator and needs no credential. Reaching a daemon
across a network needs a bearer, mTLS, or a pairing code — see
[the interface guide](https://agentd.dev/docs/interface/).

## As a library

The package's entry point is the framework-free core both UIs are built on —
the JSON-RPC/SSE wire, the event-sourced `Mirror`, and the `Observation` driver
(bootstrap → feed with cursor resume → automatic poll fallback):

```js
import { AgentdClient, Mirror, Observation } from '@agentd-dev/cli';

const client = new AgentdClient({ endpoint: 'http://127.0.0.1:8420' });
const mirror = new Mirror();
const obs = new Observation(client, mirror);
obs.start();                    // mirror.state is now a live projection
```

Build your own surface on that and it stays consistent with the shipped ones,
because they all fold the same events.

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

The protocol these clients speak is specified in
[RFC 0032](https://agentd.dev/docs/rfc-0032/).

Apache-2.0
