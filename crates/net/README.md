# agentd-net

The transport layer [agentd](https://agentd.dev) is built on, published
separately because it is useful on its own: a blocking HTTP/1.1 client and SSE
reader written against `Read + Write`, so the same request path works over TCP,
TLS, a unix socket or AF_VSOCK with no branch.

```rust
use agentd_net::http::{connect_tcp, send, Url};
use std::time::Duration;

let u = Url::parse("http://127.0.0.1:8080/health")?;
let mut s = connect_tcp(&u.host, u.port, Duration::from_secs(5))?;
let resp = send(&mut s, &u.host_header(), "GET", &u.path, &[], b"")?;
assert!(resp.is_success());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What's in it

- **HTTP/1.1 client** — request framing, chunked and content-length bodies, an
  8 MiB response cap, and CR/LF scanning on caller-supplied headers so header
  injection closes at the framing layer.
- **SSE reader** — a blocking, line-based `text/event-stream` parser that emits
  one event per blank-line separator, bounded per event.
- **TLS** (feature `tls`) — rustls with the `ring` provider and bundled
  `webpki-roots`, so a `FROM scratch` container has trust anchors without a
  system CA bundle. Mutual TLS on both sides; the server acceptor re-reads a
  rotated identity from disk without a restart.
- **SSRF guard** — `guard_host` resolves a name and refuses loopback,
  link-local, private and reserved ranges. Point it at every URL a model or a
  peer supplied.
- **X.509 field extraction** — a dependency-free DER walk lifting the subject CN
  and SANs from a verified leaf, so a SPIFFE `spiffe://` URI SAN reaches your
  own authorization rules.
- **AF_VSOCK** (feature `vsock`) — enclave and microVM transport.

## Scope

This is a *subset*, on purpose: what agentd needs, correct, with its bounds
written down. There is no connection pool, no HTTP/2, no redirect following and
no cookie jar. If you want a general-purpose client, use one — this exists
because a request path generic over `Read + Write` is worth more here than
those features.

Licensed under Apache-2.0. Source and issues:
<https://github.com/agentd-dev/source-code>.
