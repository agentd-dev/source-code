# The JSON-RPC over unix socket binding

| | |
|---|---|
| URI | `https://agentd.dev/a2a/binding/jsonrpc-unix` |
| Kind | A2A custom protocol binding (`AgentInterface.protocolBinding`) |
| Interface URL | `unix:///<absolute path of the socket>` |
| Protocol version | the A2A version the interface declares (`1.0`) |

This page is the normative specification of the URI above. It is served at
that URI. A binding carries the JSON-RPC binding's own messages, so it has no
schema bundle of its own.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
identifier like this one a SHOULD, not a MUST, and a version in a name agentd
owns would be a second name for the same thing. An incompatible change takes a
new URI with a new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's
meaning MUST NOT change under a peer that already speaks it.

## Why a binding

Two agentd instances on one host, or in one pod, can skip TCP and TLS
entirely: one listens on a unix domain socket (`a2a.listen:
unix:///run/agentd/a.sock`), and the other names that socket as a peer
endpoint. The protocol is the JSON-RPC binding, unchanged. But no `https://`
URL can name a socket path, and a stock A2A client picks an interface by its
binding and version before it sends anything. So an interface on a socket
declares this binding rather than `JSONRPC`, and a client that cannot dial a
socket never selects it.

## The binding

It is the A2A JSON-RPC binding, carried as HTTP/1.1 over a unix domain stream
socket instead of TCP. Everything that binding specifies holds unchanged:

- **Methods and params.** The same method names, the same `params`, the same
  results, including streaming (`SendStreamingMessage`, `SubscribeToTask` and
  extension methods answer with `text/event-stream`).
- **Service parameters as HTTP headers.** `A2A-Version` on every request, and
  `A2A-Extensions` on the request and its echo on the response, exactly as
  over TCP.
- **Errors.** The same JSON-RPC error codes, with the same
  `google.rpc.ErrorInfo` details, and the same HTTP status mapping: `-31401`
  travels as HTTP 401, `-31403` as HTTP 403, and every other JSON-RPC error
  is an answer delivered with HTTP 200.
- **Discovery.** The public card is a `GET /.well-known/agent-card.json` on
  the socket. Its `supportedInterfaces` entry names this binding and the
  socket's `unix:///` URL.

The HTTP `Host` header carries no meaning on a socket. A client sends any value.

## Who the peer is

The kernel is the authenticator.

- The socket file is created with mode `0600` inside a `0700` staging
  directory, then renamed into place, so no other user can reach it even
  briefly.
- On every connection, the listener reads the peer's credentials with
  `SO_PEERCRED`. Only the daemon's own uid, or root, is served. Any other
  connection is closed before a byte of HTTP is read, and logged as
  `a2a.unix.denied`.
- Every connection that passes is **the operator**. An `Authorization`
  header does not change that: the uid decided before any header was read.

A unix listener takes no TLS material (`a2a.tls` must be unset) and declares no
authentication scheme on its card. The sign-in session ops (`auth.sessions`,
`auth.sessions.revoke`) are not served on it, because it issues no sessions.

## Clients

- agentd's outbound A2A client dials a peer endpoint `unix:///<path>` over the
  socket and selects this binding from the peer's card. The dial always goes to
  the configured path, never to a URL the card names.
- A client that does not implement this binding must treat an interface
  declaring it as unsupported and choose another interface, or none. agentd's
  TypeScript display client does exactly that.

## Related

- [../a2a.md](../a2a.md): the listener, the JSON-RPC pipeline and the
  co-located peer setup.
- [command.md](command.md), [events.md](events.md),
  [task-annotations.md](task-annotations.md): the extensions, which work the
  same over this binding.
