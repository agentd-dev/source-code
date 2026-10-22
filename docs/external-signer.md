# The external signer protocol

With `identity.delegation.key_custody: external`, agentd holds none of the
keys an enrolled person's delegation rests on, with
`security.companies.key_custody: external` none of the companies preview's
seal key, and with `security.payments.key_custody: external` none of its
wallets' keys. A signer process you run holds them, and agentd asks it, over
a unix socket, to sign client assertions, to wrap and unwrap the data keys
the grant vault and the Company records are sealed with, to derive the
Company records' id keys, and to sign a payment's EIP-712 typed data. This page is the protocol's normative
specification, for operators who put a KMS, an HSM or a process of their own
behind that socket. The key words MUST, SHOULD and MAY are as in RFC 2119.

## What the signer holds

| Key | Use | Configured as |
|---|---|---|
| The IdP client key | signs the `private_key_jwt` assertion agentd presents at the IdP's token endpoint | `identity.providers.<name>.client_auth.kid` |
| Each resource's client key | signs the assertion agentd presents at a delegated server's authorization server | `mcp.servers[].auth.client_auth.kid` |
| The key-encryption key | wraps and unwraps each grant's data key, and each person's personal key | `identity.delegation.signer.seal_key` (and `seal_key_previous`) |
| The companies' key-encryption key | wraps and unwraps the data key of each User ID, Account Token and ledger record | `security.companies.signer.seal_key` |
| The companies' derivation key | derives the key each Company store's record ids are made under | `security.companies.signer.derive_key` |
| Each wallet's key | signs the EIP-3009 authorization of an x402 payment ([Wallet keys](#wallet-keys)) | `security.payments.wallets.<name>.key` |

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

The companies preview names its own signer, which may be the same socket.
Its custody is its own setting because the preview runs with or without a
delegation, and its keys — a key-encryption key and a derivation key — are
not a delegation's:

```yaml
# fragment: under security:
companies:
  key_custody: external          # local (default): seal_key, read by agentd
  signer:
    socket: /run/agentd-signer/signer.sock
    uid: 991
    seal_key: companies-kek-2026
    derive_key: companies-ids-2026
```

Under it, each User ID, Account Token and ledger record is sealed under a
data key of its own the signer wraps, bound to the record's associated data;
the key each store's record ids are made under is `derive` of a fixed label,
asked once a process, so the ids keep their shape — 64 hex digits of an
HMAC-SHA256. A store sealed under one custody does not open under the other:
changing it starts every Company over with a new User, as a new seal key
does. A signer that does not answer makes every Company unavailable until it
does; it never starts a User over.

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
| `public_key` | `key` | `alg` (`ES256`, `RS256`, or `ES256K` for a wallet key) and `jwk`, the public JWK with no private member — a wallet key's `kty: EC`, `crv: secp256k1` (RFC 8812) |
| `sign` | `key`, `alg`, `data`: the JWS signing input, `header.claims` | `sig`: the JWS signature. `ES256` is the 64-byte `R ‖ S` (convert a DER signature from a KMS); `RS256` is RSASSA-PKCS1-v1_5 with SHA-256 |
| `wrap` | `key`, `plaintext` (a 32-byte data key), `aad` | `wrapped`: opaque bytes agentd stores and hands back unchanged |
| `unwrap` | `key`, `wrapped`, `aad` | `plaintext`: the data key, only when `wrapped` was made under `key` and the same `aad` |
| `derive` | `key`, `data` (1 to 256 bytes) | `mac`: HMAC-SHA256 of `data` under the derivation key `key`, 32 bytes |
| `sign_typed_data` | `key`, `typed_data`: EIP-712 typed data as JSON, `{types, primaryType, domain, message}` as `eth_signTypedData_v4` takes it | `sig`: the 65-byte `r ‖ s ‖ v` over the typed data's EIP-712 digest, which the signer computes itself — RFC 6979, `s` in the lower half of the order, `v` 27 or 28 |

The associated data binds a wrapped key to what it seals, each part with a
four-byte big-endian length: for a grant the parts `delegation-grant-key`,
the grant id, the IdP issuer, the subject, the epoch and the refresh-token
version; for a person's personal key `delegation-personal-key`, the
principal and the key's id; for a Company record `company-record-key` and
the record's own associated data. A signer passes it to its AEAD, or to a
KMS as its encryption context. A wrapped key moved to another record does
not unwrap.

## What a signer signs

The signer's socket is reachable by any process of agentd's uid — Landlock
has no right for `connect(2)` — so the signer's own policy decides what such
a process can get from it. agentd sends the signing input itself, the JWS
`header.claims` it is about to send; a signer reads every header and claim
from it, and needs no other field. A signer:

- MUST sign only a signing input of two base64url parts, each a JSON object;
- MUST sign only a header of exactly `alg`, `typ` and `kid`, with `alg` the
  key's algorithm, `typ` `JWT` and `kid` the key id asked for;
- MUST sign only claims of exactly `iss`, `sub`, `aud`, `iat`, `exp` and
  `jti`, where `iss` equals `sub` (a client id), `aud` is a string among the
  audiences configured for that key — the authorization server's token
  endpoint or its issuer, as `assertion_aud` says — `iat` is within 120
  seconds of its clock, `exp` is after `iat` and at most 300 seconds after
  it, and `jti` is 16 to 128 base64url characters. agentd's assertions have
  `exp` sixty seconds after `iat` and a 22-character `jti`;
- MUST sign nothing with a key that has no audience configured;
- SHOULD hold `iss` to the client ids each key is registered for;
- MUST wrap and unwrap only under a key configured as a key-encryption key,
  and derive only under one configured as a derivation key;
- SHOULD derive only the labels agentd asks for: `agentd company-user record
  id`, `agentd company-account record id`, `agentd company-account index id`
  and `agentd company-ledger record id`.

What it refuses it answers `denied`, naming the rule and never the data.
agentd treats a refused signature as an unavailable signer
(`signer_unavailable`): nothing is sent to the IdP or the authorization
server. The reference signer (`agentd --internal-mock-signer`, in debug and
`internal-mocks` builds) is configured by its `keys` knob — `{"alg": "ES256",
"audiences": [...], "clients": [...]}`, `{"use": "wrap"}`, `{"use":
"derive"}` — and holds to every rule above.

## Wallet keys

Under `security.payments.key_custody: external` a wallet names a key id of
the signer (`wallets.<name>.key`): a secp256k1 key, whose `public_key` is
`ES256K` and a public JWK agentd derives the wallet's address from. agentd
sends `sign_typed_data` the whole typed data of an EIP-3009
`TransferWithAuthorization`, not a digest, so the signer reads what it signs
and computes the digest itself. agentd recovers the address from every
signature it is given and refuses one that is not the wallet's.

```yaml
# fragment: under security:
payments:
  key_custody: external          # local (default): key files sealed under seal_key
  signer:
    socket: /run/agentd-signer/signer.sock
    uid: 991
  wallets:
    ops: {key: wallet-ops-2026}
```

A signer:

- MUST sign only typed data of exactly `types`, `primaryType`, `domain` and
  `message`, whose `types` are exactly `EIP712Domain` and
  `TransferWithAuthorization`, each of exactly its members in order —
  `name string, version string, chainId uint256, verifyingContract address`
  and `from address, to address, value uint256, validAfter uint256,
  validBefore uint256, nonce bytes32` — whose `primaryType` is
  `TransferWithAuthorization`, and whose domain and message hold exactly
  those members;
- MUST sign only for a chain and token contract (`chainId`,
  `verifyingContract`) configured for that key, and only a `from` that is
  the key's own address;
- MUST refuse a `validBefore` that is not after its clock or is further
  from it than its own window, a `validAfter` not before `validBefore`, and
  a nonce that is not 32 bytes;
- SHOULD hold `to` to the payees configured for the key, and `value` to a
  maximum;
- MUST sign nothing with a wallet key that has no token configured.

What it refuses it answers `denied`; agentd fails the payment unsent
(`payment not sent: …`), and the ledger lets the step pay again later.
The reference signer's wallet keys are `{"alg": "ES256K", "assets":
[<CAIP-19 id>, …], "payees": [<address>, …], "max_value": "<atomic>",
"max_validity": <seconds>}` (`"hex"` gives the key's 32 bytes), each held to
every rule above.

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
  logged as `delegation.signer.unavailable`, and agentd starts. The
  companies' signer is asked for a derivation and a wrap and unwrap of a
  probe the same way: a refusal fails the start, and silence is
  `company.signer.unavailable`. The payments signer is asked for each
  wallet key's public key: a refusal, or a key that is not `ES256K` or not
  the address `wallets.<name>.address` names, fails the start, and silence
  is `payment.signer.unavailable` — no payment is made from those wallets
  until the signer names their addresses.
- **At use**, a delegated call whose grant cannot be opened, or whose
  assertion cannot be signed, fails `signer_unavailable` before any request
  reaches the IdP or the authorization server, and is recorded as
  `delegation.denied`. Grants are still listed while the signer is down: their
  metadata is stored in the clear.
- **An enrolment** that cannot open the grant it would replace waits for the
  signer rather than replacing a grant it never read.
- **A consent record** is sealed only under a personal key agentd already
  holds open: the person's key is unwrapped — or made and wrapped — off
  agentd's event loop, at start and when a gate of theirs opens, so a slow
  signer delays no other work. An approval whose key is not open yet decides
  nothing, the gate stays open, and the key is asked for again; a denial is
  recorded without the gate's content (`delegation.consent.unsealed`).

## Closing the socket to agentd's workers

`connect(2)` to a unix socket is no Landlock right, so a worker that runs as
agentd's uid can reach the signer and pass its peer check. Two things close
that, and both are meant to be used:

- **The signer's own policy** ([What a signer signs](#what-a-signer-signs)):
  such a process gets no signature of anything but a client assertion for a
  configured audience — which it could only present with a refresh token or
  an ID-JAG it does not have, since it cannot read the vault — and no
  unwrap of a key it has no wrapped copy of.
- **Workers under a uid of their own** (`spawn.uid`, `spawn.gid`): a worker
  then fails the peer check the signer makes of its client, and agentd's
  own. A `spawn.uid` equal to a signer's uid is refused at load. The worker
  holds no capability either, so it cannot set its uid back to agentd's.

## What it does not protect

- **The supervisor's uid reaches the socket.** A process that runs as
  agentd's own uid — agentd itself, an instance child — passes the signer's
  peer check; the signer's policy is what holds for it.
- **A record sealed under one custody does not open under the other.**
  Moving between `key_custody: local` and `external` means enrolling again,
  as losing the seal key does — and, for the companies preview, a new User
  at every Company.

## Next steps

- [Enterprise-managed authorization](authentication.md#enterprise-managed-authorization): enrolling a person and their grant.
- [Security model](security.md#acting-for-an-enrolled-person): what holds around the grant.
- [Pay for things with x402](pay-with-x402.md#keep-the-key-with-a-signer): a wallet whose key the signer holds.
- [Configuration reference](configuration.md): every `identity.delegation`, `security.companies`, `security.payments` and `spawn` key.
