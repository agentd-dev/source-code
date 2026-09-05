# RFC 0041: Encrypted instruction documents — end-to-end confidentiality

**Status:** Implemented (feature `decrypt`, default-off)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-09-05
**Extends:** RFC 0034 / RFC 0039 (instruction documents); composes with RFC 0040 (OCI transport) and the §7 signing surface.
**Depends on:** the §7 attestation stack (JOSE/Ed25519 on `ring`, shipped v1.8.0) — this reuses its crypto provider and its sign/encrypt composition rules.

---

**Normative home:** the **Instruction Specification**
(https://github.com/instruction-md/specification). Signing (§7) establishes
*authenticity and a capability ceiling*; this RFC adds the orthogonal property
signing does not: **confidentiality**. The document format and the trust ladder
are unchanged.

## 1. Summary

An instruction document is sensitive. It can embed operating policy, the exact
prompts that steer a model, references to secrets, escalation rules, and
competitive process — the crown-jewel description of how a business runs an
agent. Today that document lives in **cleartext** wherever it is stored or
served: a file on a shared volume, an `https://` object, an MCP `resource`, an
OCI blob (RFC 0040). Every operator of that storage or transport — the registry,
the CDN, the MCP server, the bucket — can read it.

This RFC adds **end-to-end confidentiality**. The document is encrypted **for a
recipient** (the agent's key) by its author or publisher; the storage and
transport see only ciphertext; **agentd decrypts on the fly** with a key it
holds, then parses the plaintext exactly as today. Confidentiality is
established between the author and the running agent — the middle never has the
plaintext. This is symmetrical to signing: signing proves *who wrote it and what
they stand behind*; encryption ensures *only the intended agent can read it*.

The mechanism is a transparent **envelope**: any source (file, `https://`, MCP,
`oci://`) may return an encrypted envelope instead of a document; agentd detects
it, decrypts, and continues. No source needs to know.

## 2. The envelope

After the bytes are fetched (§5.2 include resolution, `agent.instruction`, an
`instruction_sources` pull), agentd inspects them. They are an encrypted
envelope when either holds:

- the transport media type is `application/jose` or `…+jwe`, or the OCI layer
  media type is `application/jose; variant=instruction`; or
- the bytes begin with a recognized magic prefix — `age-encryption.org/v1` (age)
  or a compact-JWE header that decodes to `{"alg":…,"enc":…}`.

Otherwise the bytes are a plaintext instruction and nothing changes. Detection
is **fail-safe**: an envelope agentd cannot decrypt is a refusal that names the
recipient key required, never a fall-through to treating ciphertext as prose.

## 3. Formats and algorithms — the common ones, on `ring`

Two envelope formats are supported, both standard, both implementable on the
`ring` primitives already in the tree (AES-GCM, ChaCha20-Poly1305, HKDF-SHA-256,
X25519 agreement) — no new dependency:

**A. JWE Compact (RFC 7516)** — the JOSE-native choice, consistent with the §7
JWS signing stack, so a document can be *signed then encrypted* with one key
model:

| JWE field | Supported |
|---|---|
| `alg` (key management) | `ECDH-ES` (X25519 recipient, RFC 8037) and `dir` (direct shared key) |
| `enc` (content) | `A256GCM`, `A128GCM` |

*(Implementation note, v1: the `*KW` families — `A256KW`, `ECDH-ES+A256KW`,
`PBES2-*` — need the raw AES block cipher, which `ring` does not expose, and a
hand-rolled table-based AES is a cache-timing liability that fails the bar
hand-rolled X25519 passes; `C20P` never completed JOSE registration. All are
refused BY NAME with the supported set. Passphrase deployments use age's
scrypt stanzas below.)*

**B. age (v1)** — the modern file-encryption format (X25519 recipients or an
scrypt passphrase, ChaCha20-Poly1305 payload, HKDF key derivation). It is the
ergonomic "encrypt this file for these recipients" tool (`age -r <pubkey>`),
which is how operators will actually produce envelopes; agentd implements the
**decrypt** half.

The **common algorithms** map to: AES-GCM and ChaCha20-Poly1305 for content;
X25519 (ECDH) for recipients; scrypt for passphrases. All are on `ring` or a
few dozen hand-rolled lines with published test vectors (the RFC 7748 X25519
ladder — ring's agreement API is ephemeral-only and decryption needs a static
recipient key — scrypt's RFC 7914 core, bech32, base64url); no new crate.
Legacy algorithms (RSA-OAEP, PGP/OpenPGP, AES-CBC) are deliberately **out of
scope** — the modern AEAD suites cover the need without dragging in a large
asymmetric or PGP stack.

## 4. Recipient keys

agentd is a **decryption client**: it holds one or more private recipient keys
and uses the one the envelope names.

```yaml
instruction:
  decrypt:
    keys:
      - /etc/keys/agent-x25519.key      # a file (0600), or…
      - "{{secret-file:/run/secrets/agent-age.key}}"   # …a secret-ref
    # passphrase envelopes (age scrypt / JWE PBES2) resolve their passphrase
    # from a secret-ref, never inline:
    passphrase: "{{secret:instruction_passphrase}}"
```

`instruction.decrypt` is **operator surface**, restart-only, and (like
`instruction_sources`) a served `!config` may not write it — a document cannot
name the key that decrypts it. Multiple keys support rotation: the envelope's
recipient stanza selects the key; a document encrypted to the old key still
opens until it is re-published to the new one. Future: a recipient key held in
an enclave or KMS (the private key never in agentd's memory) — noted, not in v1.

## 5. Pipeline order — composing with §7

Confidentiality and authenticity compose in a fixed order, and the order is what
makes both true at once:

```
fetch  →  DECRYPT (recipient key)  →  the plaintext document
       →  verify AUTHOR signature (over the plaintext digest, §7.3)
       →  parse → trust ladder → fold → deliver (§3.5)
```

- **Sign-then-encrypt for the author signature.** The offline author signs the
  plaintext authored digest, and that signature travels **inside** the envelope
  (in the plaintext's front matter). Only a holder of the recipient key ever
  sees it — so the author attestation is itself confidential, and a middle box
  cannot even learn *who* authored the document.
- **Encrypt-then-sign for the delivery signature.** The online serving key signs
  the **ciphertext** it delivers plus the resolution manifest (§7.4). The server
  proves *what encrypted bytes it sent, to whom*, **without being able to read
  them** — which is precisely the property a per-reader resolving server must
  have when it is not trusted with plaintext.

The delivery `digest` therefore covers the **ciphertext as sent**; the author
`digest` covers the **decrypted plaintext**. Both are already in the §7 claims —
this RFC only specifies which bytes each hashes when an envelope is present.

## 6. Security model — what encryption does and does not buy

- **It buys confidentiality against the transport and storage**, and, because a
  recipient-keyed envelope can only be opened by the intended agent, a channel
  between author and agent that the middle cannot read or forge.
- **It does not buy authorization.** Decrypting a document does not admit its
  machinery — the trust ladder, the trifecta, and the operator-grant model are
  unchanged. A decrypted remote instruction is exactly as constrained as the
  same plaintext from the same source; `compute`/`infra`/`identity` still need
  their grants, and the §7.8 hard floor still bars `compose`/`identity` over the
  wire.
- **It does not replace signing.** Encryption hides content; it does not attest
  authorship unless the envelope is *also* signed (§5). An unsigned encrypted
  document is confidential but anonymous — fine for a shared-key deployment,
  insufficient where provenance matters. The two are independent and the
  operator chooses which to require per source.
- **Decryption failure is fail-closed.** A wrong key, a corrupted envelope, or
  an unsupported algorithm refuses the document naming the key/alg required —
  never a downgrade to serving ciphertext or an empty instruction.

## 7. Dependency budget

Zero new crates. AES-GCM, ChaCha20-Poly1305, HKDF and X25519 are `ring` (already
present via rustls / `aauth` / `sign`); age's bech32 and JWE's base64url + JSON
are hand-rolled / `serde_json`. The surface sits behind a default-off
**`decrypt`** cargo feature that rides `sign` (which rides `aauth` → `ring`), so
a binary that never opens an envelope carries none of it. This is the same
crypto-exception posture as `sign`: reuse the one cryptographic dependency
rustls forces, hand-roll the wire formats.

## 8. Non-goals

- **Encryption (the write half).** agentd decrypts; authors encrypt with `age`
  or a JOSE tool in CI / the control plane, to agentd's published recipient
  public key.
- **PGP / OpenPGP, RSA, AES-CBC.** Legacy algorithms are excluded; the two
  modern AEAD suites suffice and keep the moat.
- **Key distribution.** *Which* agent gets *which* recipient key is a control-
  plane / operator concern (the same place `instruction_sources` keys are
  provisioned). agentd consumes keys; it does not distribute them.
- **Encrypting agentd's own state or logs.** This RFC is about the instruction
  document in flight and at rest on foreign storage, not the file store or the
  durable log (those have their own at-rest story).

## 9. Open questions

- Whether `decrypt` is its own feature or folds into `sign` (both ride `ring`;
  the split lets a signing-only build stay smaller).
- age vs. JWE as the *default* documented format — JWE composes with the
  existing JOSE stack and the manifest; age is what operators reach for. Likely:
  support both, document age for humans and JWE for the signed/served path.
- Whether a per-reader **resolving** server (§7.4) encrypts per recipient, or
  serves one envelope encrypted to a group key — the former is stronger, the
  latter is what a CDN can cache. This interacts with the delivery signature and
  deserves its own note.
