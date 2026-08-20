# Hosting the web UI (code.agentd.dev)

The web UI is a **thin client**. Hosting it means serving three static files
from a public domain; it is not a service that talks to anyone's agent.

That distinction is the whole security argument, so it is worth stating
precisely: the page runs in the user's browser, holds their endpoint and
credential in `localStorage`, and connects **directly** to their own daemon.
The host never sees a request to an agent, never holds a token, and cannot
reach a private network. Compromising `code.agentd.dev` yields static assets.

**Do not add a backend to it.** A proxy or a session store would turn a page
that cannot leak anything into one that holds everyone's credentials and can
reach every user's daemon. The CI job asserts the bundle stays a thin client.

---

## 1. What had to change in agentd

**Private Network Access.** A page on a public origin reaching a daemon on
loopback or a LAN address is the exact shape browsers now gate. Chrome sends
`Access-Control-Request-Private-Network: true` on the CORS preflight and drops
the real request unless the answer carries
`Access-Control-Allow-Private-Network: true`. agentd did not send it, so a
hosted client failed with a CORS error naming no cause.

It does now (`crates/agentd/src/a2a/serve.rs`), and the grant is deliberately
narrow: it rides the **existing** `interface.origins` allow-list, so it says
"the origin you already configured may reach this daemon", never "any website
may". An unconfigured origin is refused before the header is considered, and
the header is not volunteered when the browser did not ask.

This ships in the daemon, so a hosted UI only works against a daemon new enough
to answer it. Below that, users get the local `agentd ui`.

## 2. What a user must configure

```yaml
interface:
  enabled: true
  origins: ["https://code.agentd.dev"]
```

Then restart. The connect screen shows this snippet with the real origin filled
in, because a CORS failure is otherwise undebuggable from the outside.

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

## 6. For the deploy

- **TLS at the edge**, HSTS there too. The container speaks plain HTTP on 8080.
- **No cookies, no auth, no logs worth keeping.** The page is anonymous; there
  is no session to protect. Access logs will show which IPs loaded a static
  page and nothing about anyone's agent.
- **Scale is a CDN problem, not a capacity one** — the payload is a few hundred
  KB and every request is cacheable except the shell.
- **Rollback is a tag change.** Images are tagged by branch, tag and short SHA.
- Consider a `security.txt` and a CSP report endpoint if you want violation
  telemetry; neither is required for it to work.

## 7. What is deliberately not here

- **No accounts, no sign-in, no server-side state.** Adding any of them changes
  the threat model from "static assets" to "holds credentials for every user's
  agent", and that is not a trade worth making for a client that works fine
  without them.
- **No proxying to daemons.** Same reason, and it would additionally make the
  host a way to reach private networks it should not be able to see.
