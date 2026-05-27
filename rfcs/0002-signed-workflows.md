# RFC 0002: Signed Workflows

**Status:** Implemented 2026-04-23 (v1 — Ed25519 detached signatures).
**Depends on:** [RFC 0001 — Harness Workflow Runtime](0001-bounded-workflow-runtime.md).
**Tracked implementation:** `crates/agentd/src/signing/` under the `signing` Cargo feature.

## Summary

Before the workflow TOML reaches the DAG validator, verify a detached
cryptographic signature over its bytes using an operator-pinned public
key. When `[signing].required = true`, a missing or invalid signature
refuses to start the runtime — **fail-closed by construction**.

This is the supply-chain answer enterprises require before they trust
a workflow runtime inside their fleet: "how do you know the TOML that
ran wasn't tampered with after your review?"

## Motivation

Today the runtime's security posture ends at the manifest's `[policy]`
block. The manifest itself is trusted by file-system convention (root
owns it, chmod 600, etc.). Three threat models that convention doesn't
cover:

- **Tampered drop.** An attacker with write access to `/etc/agent/`
  swaps `workflow.toml` for a variant that disables `[policy]` or
  widens `policy.shell.allow`.
- **Supply-chain exchange in CI.** A workflow artifact baked into a
  container image is replaced during a compromised pipeline step.
- **Multi-tenant appliance.** A mission-runner hands `agentd` a TOML
  that claims a narrow capability set but was authored by an
  un-reviewed party.

Signatures close all three: the runtime only starts if the TOML it
reads matches a signature produced by a key the operator pinned at
deploy time.

## Design

### Grammar

New top-level block in the workflow TOML:

```toml
[signing]
required = true                              # default: false
public_key_file = "/etc/agent/signing.pub"   # OR inline PEM below
public_key_pem  = """
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAzgpKg3hPm5…
-----END PUBLIC KEY-----
"""
# `algorithm` defaults to "ed25519". Only one algorithm is wired in
# v1 — ECDSA-P256 (cosign default) is a v2 expansion.
algorithm = "ed25519"
```

Exactly one of `public_key_file` / `public_key_pem` must be set.
Empty / both → spawn-time error.

### Signature format

**Detached.** A sibling file next to the workflow, `<config>.toml.sig`,
containing a single line: the base64-encoded raw 64-byte Ed25519
signature over the TOML bytes on-disk (including the final newline).

`openssl` equivalent:

```bash
# Generate key + public key (one-time).
openssl genpkey -algorithm Ed25519 -out agent-signing.key
openssl pkey -in agent-signing.key -pubout -out agent-signing.pub

# Sign a workflow.
openssl pkeyutl -sign \
    -inkey agent-signing.key \
    -rawin -in workflow.toml \
    | base64 -w 0 > workflow.toml.sig
```

For workflows embedded at build time (Mode B, `AGENTD_EMBED_CONFIG`),
the signature embeds alongside the TOML via a new
`AGENTD_EMBED_CONFIG_SIG=/path/to/workflow.toml.sig` env var read by
`build.rs`. The runtime verifies against the `[signing]` block that
was validated at build time.

### Verification flow

Invoked in `runtime::load_workflow` **before** the DAG validator:

```
1. Parse TOML → WorkflowDoc.
2. If doc.signing is present:
     a. Load pinned public key (file or inline PEM).
     b. Locate signature:
          - External config: read `<path>.sig`.
          - Embedded: read baked-in `EMBEDDED_CONFIG_SIG`.
     c. Decode base64; expect 64 bytes.
     d. Verify with ed25519_dalek::VerifyingKey::verify.
     e. On success → continue. On failure → exit 5 with a loud
        `agent: workflow signature verification failed: <reason>`.
3. If doc.signing.required = true AND no signature present → exit 5.
4. If doc.signing is absent entirely → behaviour depends on CLI /
   env overrides:
      - `AGENTD_SIGNING_REQUIRED=1` or `--signing-required` →
        refuse to start.
      - Otherwise → start with a `tracing::warn!` event on the
        `agent::audit` target noting the unsigned manifest.
```

### Failure modes

| Condition | Exit code | Audit event |
|---|---|---|
| `required=true`, `.sig` missing | 5 | `signing.sig_missing` |
| Signature decode (base64 / length) fails | 5 | `signing.sig_malformed` |
| Public key parse fails | 5 | `signing.pubkey_malformed` |
| Signature does not verify | 5 | `signing.verification_failed` |
| `required=false`, no signing block | 0 | `signing.bypassed` (warn) |
| External `--signing-required` overrides absent block | 5 | `signing.required_override` |

All failures are surfaced via `tracing::error!` on `agent::audit` with
the workflow name, the key fingerprint (SHA-256 of the raw key bytes,
hex-truncated to 16 chars) when available, and the reason.

### Key fingerprint in audit events

Every `signing.*` event carries `key_fingerprint = "<16-hex>"` so log
readers can tell which pinned key was in play without seeing the PEM.

### Algorithms

**v1 (this RFC): Ed25519.** 32-byte pubkey, 64-byte sig. Pure-Rust
verification via `ed25519-dalek` (no native C deps, aligns with the
architecture's dep-light posture — §10 `docs/agent/maturity.md`).

**v2 (future): ECDSA-P256 + Sigstore keyless.** Broader cosign
ecosystem compat. Pulls `p256` for ECDSA verification; keyless adds
sigstore transparency-log verification and is inherently async,
so it lands behind a `signing-sigstore` feature that explicitly
opts into a tokio-backed subset. Tracked as a follow-up.

### Scope — not in v1

- **Key rotation.** Operators rotate by redeploying the image /
  package / ConfigMap with the new `public_key_*`. A rotating-pinset
  feature (N accepted keys, grace window) is deferrable.
- **Multiple signers.** One pinned key per workflow. Threshold /
  MofN verification is explicit non-goal; use external policy
  (e.g. a pre-commit hook that demands N cosign signatures) if
  needed.
- **Signature metadata.** No `Signed-By`, `Expires`, etc. — the sig
  is bytes over bytes, period. Metadata belongs in the TOML.
- **OCI signature co-location.** Cosign-style signature-as-OCI-blob
  alongside an OCI artifact is v2.

### CLI / env surface

| Knob | Default | Meaning |
|---|---|---|
| `--signing-required` | off | Force `required=true` regardless of the workflow's `[signing]` block. Use in hardened deploys. |
| `AGENTD_SIGNING_REQUIRED=1` | off | Env twin of the above. |
| `--signing-key-file PATH` | none | Override the TOML's `public_key_file` at launch (rotation friendly). |
| `AGENTD_SIGNING_KEY_FILE=...` | none | Env twin. |

## Implementation

New module: `crates/agentd/src/signing/`

```
signing/
├── mod.rs              (feature gate; Config / Verifier / Error types)
├── ed25519.rs          (decode PEM / raw key, verify sig)
└── embedded.rs         (build.rs hook for baked-in sig)
```

Cargo feature:

```toml
[features]
signing = ["dep:ed25519-dalek", "dep:base64"]
```

Wire-in point: a single call in `runtime::load_workflow` gated on
`doc.signing.is_some() || overrides.signing_required`. No other
runtime code paths touch signing state — the verifier is a one-shot
at load.

## Tests

- **Unit:** PEM parse happy + malformed; base64 sig happy + bad
  length; signature-over-wrong-bytes fails.
- **Integration (cli_smoke):** end-to-end spawn with a generated
  keypair; verify the runtime refuses a tampered workflow and
  accepts a correctly-signed one.
- **Integration (build_time_validation):** `AGENTD_EMBED_CONFIG_SIG`
  baked path — tamper the TOML post-build, verify the runtime
  refuses.

## Operator workflow

```bash
# One-time: generate a signing key, pin the public half.
openssl genpkey -algorithm Ed25519 -out agent-signing.key
openssl pkey -in agent-signing.key -pubout -out agent-signing.pub

# Author a workflow.
cat > workflow.toml <<EOF
name = "production"
…
[signing]
required = true
public_key_file = "/etc/agent/signing.pub"
EOF

# Sign.
openssl pkeyutl -sign -inkey agent-signing.key \
    -rawin -in workflow.toml \
    | base64 -w 0 > workflow.toml.sig

# Ship.
scp workflow.toml workflow.toml.sig \
    agent-signing.pub prod:/etc/agent/

# Agent starts → signature verified before DAG validation.
ssh prod "sudo systemctl restart agent"
```

A tamper on the production box after this point:

```bash
# Attacker widens policy.
sudo sed -i 's/allow = \[\]/allow = ["\*"]/' /etc/agent/workflow.toml
sudo systemctl restart agent
# agent exits 5 — sig over the tampered bytes no longer verifies.
```

## Rollout plan

1. Land the feature gated off (`signing` is not in default Cargo
   features).
2. Document in `docs/agent/capabilities.md §Signing`,
   `docs/agent/operations.md §8.7`, and
   `docs/agent/maturity.md` (closes gap §2.x "supply-chain
   verification" that we'd flag but haven't named until now).
3. Bake into the Helm chart: `signing.enabled` toggles mounting the
   pubkey Secret + the `.sig` ConfigMap.
4. v2 RFC covers Sigstore + ECDSA-P256 when a real deployment asks
   for cosign-shaped signatures.

## Open questions

None active — every decision above is deliberate and reversible at a
v2 RFC. v1 lands Ed25519 + pinned-key because it's the smallest
thing that closes the threat model. Enterprises that demand Sigstore
will come back with that requirement and we'll do the work then.
