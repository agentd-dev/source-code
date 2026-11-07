# Build your own agentd

This guide is for operators who want an agentd binary or image with only the
features their configuration and workflows use. It shows how to find those
features, build them in the builder image, check what you built, and the
other ways to get agentd with a feature set you choose. After it you can ship
an image that runs your workflows and that says, in a way a registry and an
admission check can read, what it carries.

> **Note.** The builder image, `agentd --build-info` and the image's
> `dev.agentd.features` label arrived in v1.21.0. A release before it has
> none of them.

## Decide whether you need your own build

Most deployments do not. The release binaries and the image carry the
default `tls` and the release list of features
([What the release binaries carry](features.md#what-the-release-binaries-carry)),
and a feature compiled in waits for configuration: a release binary that is
not told to listen for A2A, export traces or pay does none of those. A
configuration that needs a feature the binary lacks is refused at load,
with exit code `2`, and the message names the feature:

```text
{"event":"config.invalid","msg":"workflow \"nightly\" step \"escalate\": when: CEL expressions require the 'cel' build feature"}
```

A build of your own pays off when you want one of these:

- **Less that can be reached.** Code that is not compiled in cannot be
  turned on by a configuration, an instruction document or a model. A
  binary without `x402` cannot pay, one without `companies` cannot act at a
  company, one without `exec` cannot run a local command, whatever its
  configuration says.
- **Fewer dependencies.** The release graph has 193 crates. The build this
  guide makes, with `cel`, `cron`, `metrics` and `mock-models`, has 111: the
  default build's 89 and the 22 `cel` brings. `x402` alone adds 30. Fewer
  crates means less to audit and fewer advisories that concern you.
  [Crate counts](features.md#crate-counts) has every figure.
- **A smaller binary.** Built in the builder for x86_64, the release set is
  a 15.8 MB binary in a 22.8 MB image, and the build this guide makes an
  11.0 MB binary in a 15.7 MB image.
- **A claim you can show.** "This binary cannot pay or run commands" is a
  statement `agentd --build-info` and the image's `dev.agentd.features`
  label make for you, and an admission check can enforce.
- **A feature the release leaves out.** `exec`, the local command runner, is
  never in a release binary.

Stay with the release when the features you would leave out are inert in
your configuration anyway and nobody needs the claim. A build of your own is
a build you maintain: you rebuild it for each release and each advisory, and
adding a node that needs another feature to a workflow means a new build
before the configuration loads.

## Find the features your configuration needs

Each row is configuration, or a workflow node, that needs the feature. A
binary without the feature refuses most of them at load; [Build features
and crates](features.md#configuration-that-needs-a-feature) says what each
one does without it, and gives the message.

| Your configuration or workflow has | Feature |
|---|---|
| an `https://` model, MCP server, peer, store or collector | `tls` (on by default) |
| a `schedule` start with `cron:` | `cron` |
| a `when`, `until` or `filter` guard, a computed value, or any other CEL expression (a template that is a bare path, `{{ steps.<id>.output }}`, needs none) | `cel` |
| `a2a.listen`, `a2a.peers` or `webhooks.listen`, and so a `webhook` or `a2a` start, an `a2a.send` or `a2a.delegate` step, and approvals over A2A | `a2a` |
| `mcp.servers[].events`, `event_grants`, `agent.instruction.mcp.events`, a `wait` with `on: event` that subscribes | `mcp-events` |
| a step's `payment`, `security.payments` | `x402` |
| a workflow's `act_for`, `identity.providers`, `identity.delegation`, `grant: id_jag` on an MCP server | `xaa` |
| a `companies` entry, and an `http` step with `auth: {kind: company}` that names it | `companies` |
| `auth` on a model, an MCP server, a peer or an HTTP endpoint; `security.client`; `agentd --login` | `oauth` |
| `security.aauth`, `mcp.servers[].aauth: true` | `aauth` |
| `agent.instruction.trust`, a signed instruction | `sign` |
| an encrypted instruction envelope, `agent.instruction.decrypt` | `decrypt` |
| an `oci://` instruction, prompt or template source | `oci` |
| `observability.metrics_addr`, and so `/healthz` and `/readyz` | `metrics` |
| `observability.otel` | `otel` |
| a reload on `SIGHUP` | `hot-reload` |
| `lifecycle.watch_config: true` | `config-watch` |
| a `mock:` model endpoint | `mock-models` |
| `security.exec.enabled: true` | `exec` |

Some features turn on others: `a2a`, `aauth` and `oci` turn on `tls`;
`sign` turns on `aauth`, and `decrypt` turns on `sign`; `config-watch` turns
on `hot-reload`; `companies`, `xaa` and `x402` turn on `oauth`, and
`companies` `tls` as well. A build has every feature its list turns on.

The rest of this guide builds for this configuration: a nightly workflow on
a cron schedule, with a CEL guard, and the metrics endpoint for its probes.

```yaml
agent:
  name: invoices
  instruction: Reconcile invoices against payments, and report what does not match.

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  model: gpt-5.1
  token: "{{secret:OPENAI_API_KEY}}"

store:
  kind: file
  file: { path: /var/lib/agentd }

observability:
  metrics_addr: 0.0.0.0:9464

workflows:
  - name: nightly
    steps:
      tick: { kind: schedule, cron: "0 2 * * *" }
      reconcile:
        kind: agent
        depends_on: [tick]
        instruction: Reconcile yesterday's invoices against yesterday's payments.
      escalate:
        kind: agent
        depends_on: [reconcile]
        when: "steps.reconcile.output != null"
        instruction: Write the mismatches up for the finance team.
      done: { kind: finish, depends_on: [escalate] }
```

It needs `tls`, `cron`, `cel` and `metrics`. The build adds `mock-models`,
so that you can check a turn in the image with no provider account. Leave it
out of a production build if you check against your provider instead.

## Build for your workflows

A workflow is configuration, so the binary refuses at load a workflow whose
node needs a feature it lacks, before any run starts. Build for the
workflows you run, and let the build prove it:

- **Check the configuration in the build.** `agentd-build --check-config`
  validates a configuration against the binary the build made, and fails
  the build when it does not load. A `when` guard in a build without `cel`,
  or a paying step in one without `x402`, stops the build, not the
  deployment:

  ```text
  {"event":"config.invalid","msg":"workflow \"nightly\" step \"rates\": a step's payment needs the `x402` feature (build with --features x402)"}
  agentd-build: /work/agentd.yaml does not validate against this build (above); nothing was written to /out
  ```

- **Give the check everything the configuration reads.** Copy the
  instruction file and the workflow files with the configuration, and run
  the check from the directory their relative paths start from: agentd
  resolves a relative path against its working directory. Set each
  `{{secret:NAME}}` the configuration references to a placeholder, because
  validation resolves every one.
- **Check the whole.** Each `--check-config` file is validated on its own. A
  configuration you load in layers, with several `--config` files, is
  checked as the layers; validate the whole against the image as well.
- **Workflows that arrive later are judged when they load.** A workflow
  fetched from `url:` or `uri:` at startup, one an instruction document
  served at run time declares, or one the agent stores with
  `workflow.create` is not in the build's check. agentd judges it against
  the binary it runs when it loads it. Build with every feature such a
  workflow may use.
- **Keep the check in CI.** Validate every change to the configuration
  against the image you deploy, so that a node added to a workflow fails the
  pull request rather than the rollout
  ([Validate in CI](harden.md#validate-in-ci)):

  ```bash
  docker run --rm -e OPENAI_API_KEY=placeholder -v "$PWD:/config:ro" -w /config registry.example.com/agentd:invoices --validate-config --config agentd.yaml
  ```

## Build with the builder image

The builder image is the recommended way. It holds the source of one agentd
release with every dependency vendored, the Rust toolchain, `cargo deny`
with a snapshot of the RustSec advisory database, and `agentd-build`, which
builds agentd offline with a feature set you choose from an allow-list. The
release image is built in the published builder, by its digest, with the
same `agentd-build`, so a build of your own goes through the steps the
release goes through.

| Path | What it is |
|---|---|
| `/usr/local/bin/agentd-build` | The entrypoint |
| `/usr/src/agentd` | The release's source, with `vendor/` and the `.cargo/config.toml` that points cargo at it |
| `/etc/agentd-build/features` | The features this builder offers |
| `/etc/agentd-build/builder` | The builder's name and version, which every build it makes reports. A builder built from a checkout has none |
| `/var/cache/agentd-build/target` | The target directory, with the release set compiled once |
| `/usr/local/cargo/advisory-dbs` | The advisory database snapshot `cargo deny` reads |

The toolchain is `rust:1.96-alpine`, pinned by digest, so a build is a
static musl binary. The image is about 3 GB unpacked and about 700 MB to
pull, for `linux/amd64` and `linux/arm64`.

### Get the builder image

The builder image is published as `ghcr.io/agentd-dev/agentd-builder` with
each release since v1.21.0, tagged as the image is (the full version, the
minor line, the major line and `latest`), signed with cosign and carrying an
SPDX SBOM attestation. The `Dockerfile` below pulls it.

To build it yourself instead, from a checkout: it takes about ten minutes,
most of it compiling the release set into its target directory. A builder built this
way is told no release and no commit, so a build in it reports `revision`,
`source_modified` and `builder` as `null`. The published one names its
release and its commit.

```bash
git clone https://github.com/agentd-dev/source-code.git
cd source-code
docker buildx build --target builder -t agentd-builder:local --load .
```

Docker's default builder reads local images, so pass
`--build-arg BUILDER=agentd-builder:local` to the build below. A
`docker-container` builder does not. Write the builder as an OCI layout
instead, from the checkout:

```bash
docker buildx build --target builder --output type=oci,dest=$HOME/agentd-builder.oci,tar=false .
```

and name it to the build below with `--build-context`, in the directory of
your `Dockerfile`:

```bash
docker buildx build --build-context agentd-builder=oci-layout://$HOME/agentd-builder.oci --build-arg BUILDER=agentd-builder -t agentd:invoices --load .
```

### Write the Dockerfile

Put this `Dockerfile` beside `agentd.yaml`:

```dockerfile
# syntax=docker/dockerfile:1
#
# agentd with the features this deployment needs, built in the builder image.

# The builder. Pin it by digest: replace the tag with the reference
# `docker buildx imagetools inspect` prints.
ARG BUILDER=ghcr.io/agentd-dev/agentd-builder:latest
# The features to build, and what the binary will report for them: the
# features, what each implies and the default tls, sorted.
ARG FEATURES=cel,cron,metrics,mock-models
ARG BUILD_INFO_FEATURES=cel,cron,metrics,mock-models,tls

FROM ${BUILDER} AS builder

FROM builder AS build
ARG FEATURES
ARG BUILD_INFO_FEATURES
WORKDIR /work
COPY agentd.yaml ./
# The cache mount keeps your compiles from one build to the next, seeded
# with the builder's own pre-compiled release set. The placeholder stands in
# for the secret the configuration references, which validation resolves.
RUN --mount=type=cache,target=/var/cache/agentd-build/target,from=builder,source=/var/cache/agentd-build/target \
    OPENAI_API_KEY=placeholder \
    agentd-build --features "$FEATURES" --expect-features "$BUILD_INFO_FEATURES" \
      --check-config agentd.yaml --out /out

# What agentd-build wrote, alone: export it with --target binary.
FROM scratch AS binary
COPY --from=build /out/ /

# The image.
FROM scratch
ARG BUILD_INFO_FEATURES
LABEL dev.agentd.features="${BUILD_INFO_FEATURES}"
COPY --from=build /out/agentd /agentd
USER 65532:65532
ENTRYPOINT ["/agentd"]
```

- **`FEATURES`** is what you ask for. **`BUILD_INFO_FEATURES`** is what the
  binary will report: the same list with what each feature implies and the
  default `tls`, sorted. It becomes the `dev.agentd.features` label, and
  `--expect-features` refuses the build, before it compiles, when the two
  disagree, so the label cannot misstate the binary. The refusal says what
  the value must be:

  ```text
  agentd-build: refused: this build's features are cel,cron,metrics,mock-models,tls, not cel,cron,metrics,mock-models (--expect-features)
  ```

- **The cache mount** keeps what you compile from one build to the next.
  Seed it from the builder, as here: an empty cache mount at that path hides
  the builder's pre-compiled release set, and the first build compiles
  everything. The seeded cache lasts while the builder image stays the same;
  a new builder starts again from its own.
- **The runtime stage** is the release image's: `FROM scratch`, the binary,
  uid 65532 and `/agentd` as the entrypoint. Add your own
  `org.opencontainers.image.*` labels beside `dev.agentd.features`.

### Build the image or the binary

Build the image:

```bash
docker buildx build -t agentd:invoices --load .
```

`agentd-build` says what it built, and prints the label line for the image
that carries the binary:

```text
agentd-build: warning: CI does not test this combination (cel,cron,metrics,mock-models,tls); the nearest tested set is ci.yml's row "--features cron": it adds cel,metrics,mock-models
agentd-build: cargo build --release --locked --offline -p agentd-cli --features cel,cron,metrics,mock-models
agentd-build: built agentd 1.28.0 (release, x86_64-unknown-linux-musl)
  features         cel,cron,metrics,mock-models,tls
  revision         unknown (source modified: unknown)
  builder          unknown
  tested by CI     no
  binary           /out/agentd (11009664 bytes, sha256 b82fb4ae3aad46636b4dfe3408d70f02d42751216531eb7ab4d05db1ef8cc0ff)
  build info       /out/build-info.json

The image that carries it is labelled with what it was built with:

  LABEL dev.agentd.features="cel,cron,metrics,mock-models,tls"
```

The first build of a feature set compiles the crates whose features differ
from the release set's: about five minutes on a 16-core host. A build of the
release set compiles nothing and takes seconds. A build that changes only
the configuration it checks compiles nothing either.

To get the binary alone, export the `binary` stage. It holds `agentd` and
`build-info.json`:

```bash
docker buildx build --target binary --output type=local,dest=dist .
```

Export the `binary` stage, not `build`: `build` is the whole builder,
about 3 GB, and copying it out takes a long time.

### Build without a Dockerfile

Run the builder directly for a one-off binary. It needs no network:

```bash
mkdir -p dist
docker run --rm --network none -e OPENAI_API_KEY=placeholder -v "$PWD/agentd.yaml:/work/agentd.yaml:ro" -v "$PWD/dist:/out" -w /work ghcr.io/agentd-dev/agentd-builder:latest --features cel,cron,metrics,mock-models --check-config agentd.yaml
./dist/agentd --build-info
```

The binary is static, so it runs on any Linux of its architecture. The
builder runs as root, so the files in `dist/` are root's. Each run starts
from the builder's own target directory and keeps nothing, so every run of a
set other than the release set compiles again.

### Build exec

`exec` is in the builder's `confirm` tier: a build that asks for it is
refused until it asks again with `--allow exec`.

```bash
docker run --rm --network none -v "$PWD/dist:/out" ghcr.io/agentd-dev/agentd-builder:latest --features a2a,cron,exec --allow exec
```

The refusal without `--allow` names the reason, and nothing is compiled:

```text
agentd-build: refused: exec needs --allow exec: runs local commands on the host the agent runs on (security.exec); never in a release
```

The binary still runs no command until its configuration sets
`security.exec.enabled: true` and an allow-list
([Leave exec off](harden.md#leave-exec-off)). [Build a coding
agent](coding-agent.md) is the tutorial that needs it.

## What agentd-build does

```text
agentd-build --features LIST [--no-default-features] [--allow FEATURE]...
             [--check-config FILE]... [--expect-features LIST] [--out DIR]
agentd-build --list
agentd-build --source-digest
agentd-build --help
```

| Option | Meaning |
|---|---|
| `--features LIST` | The features to build, comma-separated. Required; an empty list builds the default set alone. |
| `--no-default-features` | Leave out the default set, `tls`. The binary then reaches `https://` only through a TLS-terminating sidecar. |
| `--allow FEATURE` | Build a feature of the `confirm` tier. Repeatable; a feature of another tier is refused. |
| `--check-config FILE` | Validate `FILE` with the new binary's `--validate-config`. Repeatable; a relative path is relative to the working directory. |
| `--expect-features LIST` | Refuse the build unless its features, with what each implies and the defaults, are exactly `LIST`. |
| `--out DIR` | Where `agentd` and `build-info.json` are written. Default `/out`. |
| `--list` | The features this builder offers, their tiers, and the feature sets CI tests. |
| `--source-digest` | The digest of the source tree. |

A build runs these steps in order, and stops at the first that fails:

1. Refuse a feature the features file does not offer, a refused one, and a
   confirm one without `--allow`, over every feature the build would
   compile, the implied and default ones included.
2. Warn when the features, with what each implies, are not a set CI tests,
   and name the nearest one.
3. `cargo build --release --locked --offline -p agentd-cli` with the
   features, against the vendored source.
4. `cargo deny check`, offline, over the dependency graph of exactly this
   build.
5. Check that the binary's `--build-info` lists the features asked for.
6. Validate each `--check-config` file against the binary.
7. Write `agentd` and `build-info.json` into the output directory, and
   print the `LABEL` line for the image that carries the binary.

The exit code is `0` for a build, `2` for a refusal or a usage error before
anything compiled, and `1` for a step that failed after the build began.

### Tiers and the features file

`/etc/agentd-build/features` lists the features the builder offers, one per
line: `name<TAB>tier<TAB>reason`. `#` starts a comment.

| Tier | Meaning | Features |
|---|---|---|
| `default` | On unless the build asks for `--no-default-features` | `tls` |
| `release` | In the release set: what the published binaries and image carry | every feature of the release list |
| `tested` | Built and tested on its own in CI, outside the release set | none |
| `confirm` | Built only when asked for again with `--allow` | `exec` |
| `refused` | Never built by this builder | `internal-mocks` |

A feature with no line is not offered, and a feature the file does not offer
cannot come in through another that implies it. `agentd-build --list` prints
the file and the sets CI tests.

### Offer fewer features

To give your teams a builder that cannot build some features, make one
`FROM` the builder with a narrower features file. This one offers no
binary that can pay, act for people or run commands, and, without `oauth`,
none with OAuth credentials either:

```text
tls	default	rustls with bundled web roots
a2a	release	the A2A listener, outbound A2A peers and the webhook listener
metrics	release	/metrics, /healthz and /readyz
cron	release	five-field UTC cron expressions on schedule starts
cel	release	CEL expressions: guards, computed values, policy matches
mock-models	release	the mock: model endpoint
```

```dockerfile
# syntax=docker/dockerfile:1
FROM ghcr.io/agentd-dev/agentd-builder:latest
COPY features /etc/agentd-build/features
```

A build from it that asks for `x402` is refused twice over, for the feature
and for the `oauth` it turns on:

```text
agentd-build: refused: oauth (x402 turns it on) is not offered by this builder (agentd-build --list)
agentd-build: refused: x402 is not offered by this builder (agentd-build --list)
```

### The tested-combination warning

CI tests the release set, the default set, and each row of the feature
matrix in `.github/workflows/ci.yml`: each feature on its own, a few pairs,
and all of them. A build whose features, with what each implies, are none of
those is built, and `agentd-build` warns and names the nearest tested set
and how this build differs from it. An untested combination is not a broken
one, but nobody has run that exact mix. Validate your configuration against
the build and run a turn in it before you ship it.

### The dependency graph

`cargo deny` in the builder judges this build's own dependency graph: the
crates its features link, dev-dependencies left out. It reads the
repository's `deny.toml` for the licences, bans, sources and advisories,
with the graph replaced by this build's features, and the advisory database
snapshot the builder carries. A licence or advisory the release's
configuration does not allow fails the build, and nothing is written. Its
warnings, such as a crate in two versions, are printed and pass.

`cargo deny` refuses a snapshot older than 90 days:

```text
[ERROR] repository is stale (last update: 2026-07-16T01:27:12Z)
agentd-build: cargo deny refused this build's dependency graph (above); nothing was written to /out
```

A builder that old builds nothing. Use the builder of a newer release.

## Check what you built

Check the binary, the label and a turn before you ship the image.

**The binary's own answer.** `agentd --build-info` prints how the binary
was built:

```bash
docker run --rm agentd:invoices --build-info
```

```json
{
  "builder": null,
  "features": [
    "cel",
    "cron",
    "metrics",
    "mock-models",
    "tls"
  ],
  "name": "agentd",
  "profile": "release",
  "revision": null,
  "rustc": "rustc 1.96.1 (31fca3adb 2026-06-26)",
  "source_modified": null,
  "target": "x86_64-unknown-linux-musl",
  "version": "1.28.0"
}
```

`features` is every feature compiled in, read in the binary itself, the
implied ones included. `revision`, `source_modified` and `builder` are what
the builder was told, and `null` when it was told nothing, as a builder
built from a checkout is. A build in the published builder reports the
builder's commit as `revision` and its release as `builder`, and
`source_modified` is `true` when the source differs from that commit's.

**The label.** The image says the same without being run:

```bash
docker image inspect --format '{{ index .Config.Labels "dev.agentd.features" }}' agentd:invoices
```

```text
cel,cron,metrics,mock-models,tls
```

**A turn, with agentd as PID 1.** `--version` proves the binary starts; a
turn proves it runs an agent in the container, where it is PID 1 and its own
init ([agentd as PID 1](pid1.md)):

```bash
docker run --rm agentd:invoices --instruction "Say hello." --intelligence mock:final
```

```text
mock-llm done
```

The exit code is `0`. Without `mock-models`, run the turn against your
provider instead.

**The builder image.** Verify the builder's signature and SBOM before you
build with it. They are made by the release
workflow, as the image's are:

```bash
cosign verify ghcr.io/agentd-dev/agentd-builder:latest --certificate-identity-regexp '^https://github\.com/agentd-dev/source-code/\.github/workflows/release\.yml@refs/tags/v' --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation --type spdxjson ghcr.io/agentd-dev/agentd-builder:latest --certificate-identity-regexp '^https://github\.com/agentd-dev/source-code/\.github/workflows/release\.yml@refs/tags/v' --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The identity is the release workflow at a release tag. To accept one
release only, name it exactly:
`--certificate-identity https://github.com/agentd-dev/source-code/.github/workflows/release.yml@refs/tags/v1.28.0`.

## Other ways to get agentd

### The release binaries and image

When the release set covers your configuration, take a release and write
the configuration. Nothing to build, and the image is signed:

```bash
curl -fsSL https://agentd.dev/install.sh | sh
docker run --rm ghcr.io/agentd-dev/agentd:1.28.0 --version
```

[Install](install.md) covers both, and how to verify them.

### Build the repository's Dockerfile from its URL

BuildKit builds a Git repository by its URL, with no checkout. The
repository's `Dockerfile` takes the features as a build argument:

```bash
docker buildx build --build-arg FEATURES=cel,cron,metrics,mock-models --build-arg BUILD_INFO_FEATURES=cel,cron,metrics,mock-models,tls -t agentd:invoices --load https://github.com/agentd-dev/source-code.git#v1.28.0
```

Use it for a one-off image when you have Docker and nothing else. It
compiles everything from the start, about five minutes for this feature
set, and it fetches what it builds from, so it needs the network. It cannot
check your configuration.

The `Dockerfile` builds through its builder stage: this build refuses a
feature outside the allow-list, `exec` among them, because it passes no
`--allow`; it runs `cargo deny`; and it labels the image with
`dev.agentd.features`. Before v1.21.0 it compiled `FEATURES` as given, with
none of those, and ignored `BUILD_INFO_FEATURES`.

### Install the published crate

With a Rust toolchain of at least 1.96 and a C compiler, install
`agentd-cli` from crates.io with the features you want:

```bash
cargo install agentd-cli --locked --features "cel,cron,metrics,mock-models"
```

Use it on a workstation or a CI runner that already has Rust. It builds a
published version only, against the network, into a dynamically linked
binary for the host. A feature the published crate does not have is
refused by cargo (`does not have these features`): `companies`, `xaa` and
`x402` are in the published crate since v1.21.0.

### Build a checkout

Build a checkout for a revision that is not released, or a source you
changed:

```bash
git clone https://github.com/agentd-dev/source-code.git
cd source-code
cargo build --release --locked -p agentd-cli --features "cel,cron,metrics,mock-models"
./target/release/agentd --build-info
```

`--build-info` reports the revision, whether the source was modified and
the builder only when the build is told them. Tell it from the checkout:

```bash
export AGENTD_REVISION="$(git rev-parse HEAD)"
export AGENTD_SOURCE_MODIFIED="$(test -z "$(git status --porcelain)" && echo false || echo true)"
export AGENTD_BUILDER="ci.example.com"
cargo build --release --locked -p agentd-cli --features "cel,cron,metrics,mock-models"
```

`AGENTD_SOURCE_MODIFIED` is `true` or `false`; any other value fails the
build. A host build is dynamically linked. For a static musl binary, build
in the builder image, or with `cross` for the
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` targets as the
release does.

### Embed agentd-core

When you want your own Rust functions as tools the model calls, depend on
`agentd-core` and build your own binary with the features in your
`Cargo.toml`. `--build-info` is then yours to answer: `agentd-core` hands it
to your binary as `Ask::BuildInfo`, and only your crate knows its features.
[Embed agentd in Rust](embedding.md) is the guide.

## Compare the ways

| Way | You need | Allow-list | Offline and pinned | `--build-info` reports | Signed, with an SBOM | Time |
|---|---|---|---|---|---|---|
| Release binary or image | nothing | — | the image, by digest | features; the image also its revision and builder | the image | a download |
| Builder image | Docker with BuildKit | yes, with `--allow` and a features file you can narrow | yes: vendored, `--locked`, the builder pinned by digest | features, revision, builder | the builder; your image is yours to sign | about five minutes for a new feature set; seconds for the release set, or again with the cache mount |
| Repository `Dockerfile` by URL | Docker with BuildKit | yes, with no `--allow` | `--locked`, but it fetches | features | no | about five minutes, all of it a compile |
| `cargo install` | Rust 1.96, a C compiler | no | `--locked`, but it fetches | features | no | about four minutes |
| `cargo build` of a checkout | Rust 1.96, a C compiler | no | `--locked`; offline once vendored | features, and what you export | no | about four minutes |
| Embedding `agentd-core` | Rust 1.96, a C compiler | no | your build's | what your binary reports | no | your build's |

The times are for a build with `cel`, `cron`, `metrics` and `mock-models` on
a 16-core amd64 host. The builder image, the allow-list of the repository's
`Dockerfile` and `--build-info` are in v1.21.0 and later; an older binary
does not answer `--build-info`.

## Best practices

- **Pin the builder by digest.** A tag moves; a digest does not. Print the
  reference to pin, and write it into the `Dockerfile`'s `BUILDER`:

  ```bash
  docker buildx imagetools inspect ghcr.io/agentd-dev/agentd-builder:latest --format '{{.Name}}@{{.Manifest.Digest}}'
  ```

  The digest is the multi-platform index's, so each platform resolves its
  own builder under it.
- **Keep the inputs fixed.** A build in the builder is `--locked` and
  offline, against the vendored source and a toolchain pinned by digest, so
  the same builder digest and the same features give the same binary, byte
  for byte, whether the build reuses a cache or compiles from the start.
  Keep the `Dockerfile`, the features and the builder digest in version
  control, and compare the `sha256` `agentd-build` prints.
- **Enforce on the consuming side.** The allow-list keeps a builder from
  building what it does not offer. It does not stop anyone from building
  agentd another way, so it is a convenience, not a security boundary.
  Enforce what you allow where images are admitted: verify your image's
  signature, then read its label. A check before a deploy, with your own
  CI's signing identity:

  ```bash
  img=registry.example.com/agentd:invoices
  cosign verify "$img" --certificate-identity https://github.com/example/agents/.github/workflows/image.yml@refs/heads/main --certificate-oidc-issuer https://token.actions.githubusercontent.com > /dev/null || exit 1
  features=$(docker buildx imagetools inspect "$img" --format '{{json .Image}}' | jq -r 'if has("config") then . else .["linux/amd64"] end | .config.Labels["dev.agentd.features"] // ""')
  case ",$features," in
    ,,) echo "refused: $img has no dev.agentd.features label"; exit 1 ;;
    *,exec,* | *,x402,*) echo "refused: $img carries $features"; exit 1 ;;
  esac
  ```

  The label is part of the image the signature covers, so it is as true as
  whoever signed the image. An admission controller can make the same two
  checks.
- **Test an untested combination yourself.** CI has not run your exact
  mix. Validate the configuration against the build, run a turn and a run
  of each workflow in it, and do it again for each new builder.
- **Rebuild for a release and for an advisory.** A builder carries one
  release and an advisory snapshot that ages: rebuild with the newer builder
  rather than patching an old one.
- **Offer the source with what you distribute.** agentd is licensed
  AGPL-3.0-only. If you give your build, a binary or an image, to others,
  the licence requires you to offer them its corresponding source; if you
  change agentd and people use your changed version over a network, it
  requires you to offer them the changed source. The builder image carries
  the source of what it builds, dependencies included, in
  `/usr/src/agentd`, and `source_modified` says whether a build changed it.
  This is a summary, not legal advice: the licence is in `LICENSE`.
  Commercial licensing is available from the project at agent@agentd.dev.

## Next steps

- [Build features and crates](features.md): every feature, what it adds,
  and the crates.
- [Deploy](deployment.md): run the image as a job, a daemon or on
  Kubernetes.
- [Harden a deployment](harden.md): the configuration side of a small
  attack surface.
- [Embed agentd in Rust](embedding.md): your own binary on `agentd-core`.
