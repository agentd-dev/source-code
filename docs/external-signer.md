# The external signer protocol

With `identity.delegation.key_custody: external`, agentd holds none of the
keys an enrolled person's delegation rests on. A signer process you run
holds them, and agentd asks it, over a unix socket, to sign client assertions
and to wrap and unwrap the data keys the grant vault is sealed with. This page
is the protocol's normative specification, for operators who put a KMS, an
HSM or a process of their own behind that socket.

## What the signer holds

| Key | Use | Configured as |
|---|---|---|
| The IdP client key | signs the `private_key_jwt` assertion agentd presents at the IdP's token endpoint | `identity.providers.<name>.client_auth.kid` |
| Each resource's client key | signs the assertion agentd presents at a delegated server's authorization server | `mcp.servers[].auth.client_auth.kid` |
| The key-encryption key | wraps and unwraps each grant's data key | `identity.delegation.signer.seal_key` (and `seal_key_previous`) |

A key is named by its id, and a signing key's id is also the `kid` its
assertions carry, so register the public key at the IdP under that `kid`.
Each write of a grant draws a fresh 32-byte data key, seals the refresh token
with it under AES-256-GCM, and keeps it only as the signer wrapped it. Before
agentd asks the IdP anything that may rotate the refresh token, it draws and
wraps the data key of the record that would keep the new token, so a rotated
token is kept even when the signer stops answering after the IdP has spent the
old one. agentd wipes the data key once the record is sealed or opened, and
the buffers it passes through on the way: the request field and frame it
sends, the answer frame and field it reads. The AES key schedule that `ring`
derives from it is not wiped.

```yaml
# fragment: under identity:
providers:
  okta:
    issuer: https://acme.okta.com
    profile: okta
    client_id: 0oa9agentdXAA
    client_auth: {method: private_key_jwt, kid: agentd-idp-2026, alg: ES256}
delegation:
  key_custody: external
  signer:
    socket: /run/agentd-signer/signer.sock
    uid: 991
    timeout: 5s
    seal_key: vault-kek-2026
```

## Transport

- **The socket.** A unix stream socket at the absolute path `signer.socket`.
  agentd connects for each request and closes the connection after the
  answer. A signer may also serve several requests on one connection, in
  turn.
- **The peer.** Before it sends a byte, agentd reads the socket's peer
  credentials (`SO_PEERCRED`; `getpeereid` off Linux) and refuses a peer
  whose uid is not `signer.uid`. Nothing is sent, and the start or the call
  fails, naming both uids. A signer should check its client the same way:
  accept only agentd's uid.
- **Run the signer as another uid.** The peer check tells the signer apart
  from agentd's workers only when `signer.uid` is not agentd's own, and a
  signer of agentd's uid can have its memory and key files read by them. A
  `signer.uid` equal to agentd's is a load warning. Put the socket in a
  directory of its own that agentd's uid cannot write, not in `/tmp`. Under Landlock agentd's children are
  also denied the socket's path, so a child can neither remove the socket nor
  bind another in its place, though it can still connect.
- **A frame** is a four-byte unsigned big-endian length, then that many bytes
  of one UTF-8 JSON object. A frame is 1 to 65 536 bytes; a longer one is a
  protocol error, and the connection is closed.
- **Bytes** inside a message (data, signatures, keys, associated data) are
  base64url without padding.

## Messages

Every request carries an `op`, and every answer `ok`.

```json
{"op": "sign", "key": "agentd-idp-2026", "alg": "ES256", "data": "eyJhbGciOiJFUzI1NiIs…"}
```

```json
{"ok": true, "sig": "q3VxH8wZ…"}
```

```json
{"ok": false, "error": {"code": "unknown_key", "message": "no key \"agentd-idp-2026\""}}
```

| Op | Request | Answer |
|---|---|---|
| `public_key` | `key` | `alg` (`ES256` or `RS256`) and `jwk`, the public JWK with no private member |
| `sign` | `key`, `alg`, `data`: the JWS signing input, `header.claims` | `sig`: the JWS signature. `ES256` is the 64-byte `R ‖ S` (convert a DER signature from a KMS); `RS256` is RSASSA-PKCS1-v1_5 with SHA-256 |
| `wrap` | `key`, `plaintext` (a 32-byte data key), `aad` | `wrapped`: opaque bytes agentd stores and hands back unchanged |
| `unwrap` | `key`, `wrapped`, `aad` | `plaintext`: the data key, only when `wrapped` was made under `key` and the same `aad` |

The associated data binds a wrapped key to its grant: the parts
`delegation-grant-key`, the grant id, the IdP issuer, the subject, the epoch
and the refresh-token version, each with a four-byte big-endian length. A
signer passes it to its AEAD, or to a KMS as its encryption context. A wrapped
key moved to another grant's record does not unwrap.

A signer may read the signing input before it signs. It is a JWS whose
claims are `iss` and `sub` (the client id), `aud` (the token endpoint or the
issuer), `iat`, `exp` sixty seconds later and a random `jti`. A signer that
signs only an assertion of that shape, for audiences it knows, is a narrower
oracle than one that signs anything.

## Errors

| Code | Meaning |
|---|---|
| `unknown_key` | The signer holds no key of that id. |
| `wrong_use` | The key is not one for this op: a key-encryption key asked to sign, a signing key asked to wrap. |
| `wrong_alg` | The key is not of the algorithm asked. |
| `unwrap_failed` | `wrapped` does not open under `key` and `aad`. agentd treats the grant record as unreadable. |
| `bad_request` | A field is missing or not base64url, or the op is unknown. |
| `denied` | The signer's own policy refuses the request. |
| `internal` | Anything else on the signer's side. |

## When the signer does not answer

agentd fails closed. A request ends within `signer.timeout` (default 5s):
the connect, the one retry, the write and the read together. On Linux the
connect is bounded too, so a signer whose listen backlog is full, or that is
stopped, does not hold agentd. Each read and write is given only what is left
of the timeout, so an answer sent a byte at a time is cut off. A connection
that could not be made (no socket, nobody listening) is tried once more after
100 ms, while time allows; a request the signer may have received is never
sent again.

- **At start**, agentd asks for the public key of every configured signing
  key, checks its algorithm, and wraps and unwraps a probe under the seal key.
  A signer that refuses (an unknown key, a wrong algorithm, the wrong uid) is
  a configuration error, and the start fails. A signer that does not answer is
  logged as `delegation.signer.unavailable`, and agentd starts.
- **At use**, a delegated call whose grant cannot be opened, or whose
  assertion cannot be signed, fails `signer_unavailable` before any request
  reaches the IdP or the authorization server, and is recorded as
  `delegation.denied`. Grants are still listed while the signer is down: their
  metadata is stored in the clear.
- **An enrolment** that cannot open the grant it would replace waits for the
  signer rather than replacing a grant it never read.

## What it does not protect

- **A process of agentd's uid can reach the socket.** `connect(2)` to a unix
  socket is no Landlock right, so a worker running as agentd's uid could ask
  the signer to sign. It cannot read the vault, so it has no refresh token to
  present and no wrapped key to unwrap ([Security model](security.md)). Run
  workers under another uid, or have the signer sign only what an assertion
  is, to close that.
- **A record sealed under one custody does not open under the other.**
  Moving between `key_custody: local` and `external` means enrolling again,
  as losing the seal key does.
- **The companies preview's seal key** (`security.companies.seal_key`) is
  read locally: it also derives the record ids of the User ID store, which an
  envelope-encryption signer does not offer.

## Next steps

- [Enterprise-managed authorization](authentication.md#enterprise-managed-authorization): enrolling a person and their grant.
- [Security model](security.md#acting-for-an-enrolled-person): what holds around the grant.
- [Configuration reference](configuration.md): every `identity.delegation` key.
