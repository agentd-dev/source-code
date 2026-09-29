# Hosting the web UI (code.agentd.dev)

The web UI is a **thin client**. Hosting it means serving three static files
from a public domain; it is not a service that talks to anyone's agent.

That distinction is the whole security argument, so it is worth stating
precisely: the page runs in the user's browser and connects **directly** to
their own daemon. It remembers the endpoint in `localStorage`, and keeps the
session it signs in with in `sessionStorage` — one tab, one visit — and nowhere
else. The host never sees a request to an agent, never holds a token, and
cannot reach a private network. Compromising `code.agentd.dev` yields static
assets.

**Do not add a backend to it.** A proxy or a session store would turn a page
that cannot leak anything into one that holds everyone's credentials and can
reach every user's daemon. The CI job asserts the bundle stays a thin client.

---

## 1. What the daemon has to grant

**The origin.** The daemon answers a web page only from an origin listed in
`a2a.cors.origins`, compared exactly (scheme, host, port). A request from any
other origin — `Origin: null` included — is refused `403` before its body is
read. There is no implicit trust for loopback: a UI served from
`http://127.0.0.1:4173` is listed like any other. The one origin admitted
unlisted is the page `agentd ui` itself launches, and only by that launcher's
daemon, for as long as it runs.

**Private Network Access.** A page on a public origin reaching a daemon on
loopback or a LAN address is the exact shape browsers gate. Chrome sends
`Access-Control-Request-Private-Network: true` on the CORS preflight and drops
the real request unless the answer carries
`Access-Control-Allow-Private-Network: true`. Without that answer a hosted
client fails with a CORS error that names no cause.

agentd answers it (`crates/agentd/src/a2a/serve/cors.rs`), and the grant is
deliberately narrow: it rides the `a2a.cors.origins` list, so it says "the
origin you already configured may reach this daemon", never "any website may".
An unlisted origin is refused before the header is considered, and the header
is not volunteered when the browser did not ask. The Agent Card is the one
document any origin may read — it is public — but even there Private Network
Access is granted only to a listed origin.

**A way to sign in.** A browser is never the daemon's implicit operator: a
request carrying `Origin` always authenticates, even on a loopback daemon with
nothing configured. A hosted page signs in with the **device grant**, so the
daemon needs `a2a.device_grant` — which in turn needs an operator credential to
approve with (`a2a.bearer`, or an operator `bearer_ref` rule). A daemon with no
credential at all issues browser sessions only to the tab `agentd ui` opens;
the hosted page says so rather than failing.

## 2. What a user must configure

```yaml
a2a:
  listen: http://127.0.0.1:8420
  bearer: "{{secret:AGENTD_OPERATOR_TOKEN}}"   # the operator who approves sign-ins
  cors:
    origins: ["https://code.agentd.dev"]
  device_grant:
    enabled: true
```

List every origin a UI is served from — `https://code.agentd.dev`, an
`agentd-ui` you run by hand (`http://127.0.0.1:4173`), a copy on your own
domain — except the one `agentd ui` launches. `a2a.cors.origins` is reloadable;
the device grant takes a restart. The connect screen shows the `cors` snippet
with the page's real origin filled in, because a CORS failure is otherwise
undebuggable from the outside.

Then the person connects: the page reads the card, offers **sign in**, and
shows a code; an operator approves it under a name (`/approve <code> <name>`
from a TUI or another signed-in tab). The session lives in that tab's
`sessionStorage`, bound to the endpoint it was issued for, until it expires or
the person runs `/disconnect`, which revokes it. `localStorage` keeps only the
endpoint and the chrome layout, and no credential is ever read from or put in
a URL. See [interface.md](interface.md#signing-in).

## 3. Browser support — and the one that does not work

| Browser | Works | Why |
|---|---|---|
| Chrome, Edge | yes | needs the PNA grant above |
| Firefox | yes | treats loopback as a secure origin; no PNA preflight |
| **Safari** | **no** | the one browser that blocks an HTTPS page from reaching `http://localhost` |

Safari's block cannot be worked around from the page. The honest answers are:
run the daemon behind TLS so the connection is HTTPS-to-HTTPS, or use
`agentd ui` locally. The connect screen says so rather than failing silently.

## 4. The artifacts

`.github/workflows/hosted-ui.yml` produces both on every push to `main` and on
tags:

- **`agentd-ui-static.tar.gz`** + `SHA256SUMS.ui` — extract onto any static
  host or CDN.
- **`ghcr.io/agentd-dev/agentd-ui`** — `linux/amd64` and `linux/arm64`, nginx
  serving the bundle, unprivileged on **:8080**, with `/healthz`.

The job typechecks, tests, asserts no secret-shaped reference reached the
bundle, and smoke-tests the built image — including that the CSP header is
actually present, because nginx silently drops inherited headers in any
`location` that declares its own (see §5).

## 5. Serving it

`interface/deploy/nginx.conf` is the reference config. Three things matter:

**The CSP.** `connect-src` is deliberately wide — the product is a page that
connects to a daemon at an address only the user knows, so it cannot be
enumerated. Everything else is closed to compensate: `default-src 'none'`,
`script-src 'self'` with no `unsafe-inline`, `frame-ancestors 'none'`. No
third-party script can run, which is what makes the wide `connect-src`
acceptable rather than reckless.

**Header inheritance.** nginx inherits `add_header` only into blocks that
declare none of their own. Setting cache-control with `add_header` in a
`location` silently drops every security header — which is how a site ships
without the CSP it appears to configure. The reference config uses `expires`
for caching, which does not suppress inheritance. This is asserted in CI.

**Caching.** The shell must not be cached (`expires -1`) or a deploy strands
users on an old bundle; the JS and CSS may be (`expires 7d`).

A hosted copy serves no `bootstrap.json`, so the page names no endpoint of its
own: it starts at the connect form, pre-filled from `?endpoint=…` or from the
endpoint this browser remembers.

## 6. The local server, `agentd-ui`

The same bundle, served next to the daemon by `agentd-ui` (and started for you
by `agentd ui`), is held to a tighter policy, because it knows exactly one
endpoint:

- It binds `127.0.0.1` only (`--port N`, default 4173, or a socket inherited
  with `--listen-fd N`), and answers only a `Host` of `127.0.0.1:<port>` or
  `localhost:<port>`, so a page that rebinds its own name to 127.0.0.1 cannot
  read it.
- **`/bootstrap.json`** is `{endpoint}` and nothing more, `Cache-Control:
  no-store`, and is served only to the page's own `fetch`: `Sec-Fetch-Site`
  must be `same-origin` or `none`, and an `Origin`, when present, must be this
  server's.
- Every response carries `Cross-Origin-Resource-Policy: same-origin`,
  `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and the
  CSP `default-src 'self'; connect-src 'self' <the endpoint's origin>;
  frame-ancestors 'none'; base-uri 'none'; form-action 'none'` — the card, the
  sign-in and every JSON-RPC call go to the endpoint's origin, and nowhere
  else.
- It holds no credential and reads none from its environment or a URL.
  `--open` opens the plain page, which signs itself in; a tab that is already
  signed in is `agentd ui`'s to open, with a single-use code in the URL
  fragment ([interface.md](interface.md#launcher)).

## 7. For the deploy

- **TLS at the edge**, HSTS there too. The container speaks plain HTTP on 8080.
- **No cookies, no auth, no logs worth keeping.** The page is anonymous; there
  is no session on the host to protect. Access logs will show which IPs loaded
  a static page and nothing about anyone's agent.
- **Scale is a CDN problem, not a capacity one** — the payload is a few hundred
  KB and every request is cacheable except the shell.
- **Rollback is a tag change.** Images are tagged by branch, tag and short SHA.
- Consider a `security.txt` and a CSP report endpoint if you want violation
  telemetry; neither is required for it to work.

## 8. What is deliberately not here

- **No accounts, no sign-in, no server-side state on the host.** Sign-in is
  between the tab and the user's own daemon. Adding any of these to the host
  changes the threat model from "static assets" to "holds credentials for
  every user's agent", and that is not a trade worth making for a client that
  works fine without them.
- **No proxying to daemons.** Same reason, and it would additionally make the
  host a way to reach private networks it should not be able to see.
