# Install

This page covers every way to get agentd onto a machine: the install script,
a release download, the container image, the terminal and web clients, and a
build from source. It is for anyone setting agentd up for the first time.
When you finish, `agentd --version` answers, and you know which build the
later tutorials need.

## Choose a way to install

| Way | What you get | Use it when |
|---|---|---|
| [Install script](#install-with-the-script) | The release binary, checksum-verified, on your `PATH` | A Linux workstation, VM or CI runner |
| [Release download](#download-a-release-by-hand) | The same binary, fetched and checked by you | You mirror artifacts, or want every step in your own hands |
| [Container image](#run-the-container-image) | `ghcr.io/agentd-dev/agentd`, multi-arch, signed | Docker, Kubernetes, any OCI runtime |
| [npm clients](#install-the-terminal-and-web-clients) | `agentd-tui` and `agentd-ui` | You want to talk to a running agent |
| [Source build](#build-from-source) | Any feature set, including `exec` | The coding-agent tutorial, a custom feature set, a platform the releases do not cover |

Most people need two of these: the binary (script, download or image) and
the npm clients.

## Platforms

agentd publishes Linux binaries only, for `x86_64` and `aarch64`. They are
static musl builds with no runtime dependencies, so one file runs on any
Linux distribution of either architecture.

There are no macOS or Windows binaries, and CI builds and tests on Linux
only. On Windows, use WSL2. On macOS, run the container image; the install
script prints a source-build recipe there, but that platform is untested.

## Install with the script

The script detects your architecture, downloads the latest release, checks
it against the release's `SHA256SUMS`, runs it once to make sure it works on
this machine, and installs it:

```bash
curl -fsSL https://agentd.dev/install.sh | sh
```

```text
agentd downloading agentd-v1.20.0-x86_64-unknown-linux-musl.tar.gz ...
agentd checksum ok
agentd installed agentd 1.20.0 to /usr/local/bin/agentd
```

It installs to `/usr/local/bin` when that is writable and to
`~/.local/bin` otherwise, and it never runs `sudo` for you. When the
directory is not on your `PATH`, it says so and prints the `export` line to
add.

Pass options after `sh -s --`, or set the matching environment variable:

| Option | Variable | Effect |
|---|---|---|
| `--version <tag>` | `AGENTD_VERSION` | Install that release instead of the latest, for example `v1.20.0` |
| `--dir <path>` | `AGENTD_INSTALL_DIR` | Install into that directory |
| `--no-verify` | `AGENTD_NO_VERIFY=1` | Skip the checksum check (it warns that it did) |

```bash
curl -fsSL https://agentd.dev/install.sh | sh -s -- --version v1.20.0 --dir "$HOME/bin"
```

The script needs `curl` or `wget`, and `tar`. It verifies with `sha256sum`
or `shasum`; with neither, it warns and installs unverified. It asks the
GitHub API for the latest tag and falls back to the `releases/latest`
redirect when the API's hourly quota is spent, which is common on shared CI
hosts. A checksum mismatch stops the install.

## Download a release by hand

Every release publishes one archive per architecture and a `SHA256SUMS` file
at `https://github.com/agentd-dev/source-code/releases`. Set the tag and the
architecture (`x86_64` or `aarch64`), then fetch, check and unpack:

```bash
TAG=v1.20.0
ARCH=x86_64
curl -fsSLO "https://github.com/agentd-dev/source-code/releases/download/$TAG/agentd-$TAG-$ARCH-unknown-linux-musl.tar.gz"
curl -fsSLO "https://github.com/agentd-dev/source-code/releases/download/$TAG/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar xzf "agentd-$TAG-$ARCH-unknown-linux-musl.tar.gz"
./agentd --version
```

```text
agentd-v1.20.0-x86_64-unknown-linux-musl.tar.gz: OK
agentd 1.20.0
```

The archive holds a single file, `agentd`. Move it anywhere on your `PATH`:

```bash
install -m 0755 agentd "$HOME/.local/bin/agentd"
```

## Run the container image

The image is `ghcr.io/agentd-dev/agentd`, built for `linux/amd64` and
`linux/arm64` from the same feature set as the release binaries:

```bash
docker run --rm ghcr.io/agentd-dev/agentd:latest --version
```

Each release is tagged by its full version (`1.20.0`), its minor line
(`1.20`) and its major line (`1`), and the newest is also `latest`. Pin a
full version or a digest for anything you deploy.

The image is `FROM scratch`: it holds `/agentd` and nothing else, no shell
and no libc. It runs as uid 65532, and its entrypoint is `/agentd`, so
everything after the image name is an agentd argument. Pass secrets as
environment variables and mount configuration read-only. With a
configuration file `agent.yaml` in the current directory:

```bash
docker run --rm -e OPENAI_API_KEY \
  -v "$PWD:/config:ro" \
  ghcr.io/agentd-dev/agentd:latest --validate-config --config /config/agent.yaml
```

A long-lived container also needs somewhere to keep its state;
[Deploy](deployment.md) covers volumes, probes and Kubernetes.

Each image is signed with cosign (keyless, from the release workflow) and
carries an SPDX SBOM attestation. Verify both before you trust a tag:

```bash
cosign verify ghcr.io/agentd-dev/agentd:latest \
  --certificate-identity-regexp '^https://github.com/agentd-dev/source-code/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation --type spdxjson ghcr.io/agentd-dev/agentd:latest \
  --certificate-identity-regexp '^https://github.com/agentd-dev/source-code/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

When a check passes, the command's report includes:

```text
The following checks were performed on each of these signatures:
  - The cosign claims were validated
  - Existence of the claims in the transparency log was verified offline
  - The code-signing certificate was verified using trusted certificate authority certificates
```

## Install the terminal and web clients

The display clients are a separate npm package, `@agentd-dev/cli`. It needs
Node.js 20 or newer and installs two commands, `agentd-tui` (the terminal
UI) and `agentd-ui` (the web UI):

```bash
npm install -g @agentd-dev/cli
```

They are thin clients: the agent's state lives in the daemon, and a client
only renders it. `agentd tui` and `agentd ui` start a daemon and a client
together and find the client on your `PATH`; set `AGENTD_TUI_BIN` or
`AGENTD_UI_BIN` to point them somewhere else. [Use the terminal and web
UI](interface.md) covers both.

## Build from source

Build from source when you need a feature the release binaries leave out,
or a platform they do not cover. You need Rust 1.96 or newer and a C
compiler: `ring`, the crypto library, compiles a few C files, and `rustc`
uses the compiler as its linker on Linux. No `cmake` and no C++ compiler.

Install the published crate with the features you want:

```bash
cargo install agentd-cli --locked --features "a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models"
```

Or build a checkout. The binary lands in `target/release/agentd`:

```bash
git clone https://github.com/agentd-dev/source-code.git
cd source-code
cargo build -p agentd-cli --release --locked --features "a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models"
./target/release/agentd --version
```

The feature list above is the release set. It includes `mock-models`, the
mock model endpoint (`mock:final`, `mock:file:<playbook.json>`) that answers
with no key and no network, so you can try an agent or a workflow without a
model provider.

A host build like this one is a dynamically linked Linux executable. The
static musl binaries the releases ship are built with `cross` for the
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` targets, and the
repository's `Dockerfile` builds the same static binary into a `scratch`
image (`docker build --build-arg FEATURES=… .`).

Two features a guide needs are never in a release binary:

| Feature | What it adds | Needed by |
|---|---|---|
| `exec` | The guarded local command runner, also switched on by `security.exec.enabled` | [Build a coding agent](coding-agent.md) |
| `companies` | Acting at companies for one person under the Personal Agent Protocol draft, a preview | [Act at companies for a person](act-at-companies.md) |

Add one to the list, for example `--features "a2a,…,mock-models,exec"`.

Every feature and the configuration that needs it is in [Build features and
crates](features.md).

## Verify the install

Check the version:

```bash
agentd --version
```

```text
agentd 1.20.0
```

Then validate a configuration without running anything. This one names a
model endpoint and a task, and nothing is called:

```bash
agentd --validate-config \
  --instruction "Say hello." \
  --intelligence https://api.openai.com/v1/chat/completions \
  --model gpt-5.1
```

```text
{"event":"config.warning","msg":"store.kind is none: this one-shot run is not durable (a crash re-runs it from scratch); set store.kind for durability"}
{"event":"config.valid","files":[]}
```

The exit code is `0` for a valid configuration and `2` for an invalid one.
The warning is expected: a one-shot run keeps no state.

To see a run end to end with no provider, point the same task at the mock
model endpoint:

```bash
agentd --instruction "Say hello." --intelligence mock:final 2> /dev/null
```

```text
mock-llm done
```

`agentd --capabilities` prints a JSON manifest of what this binary and its
configuration provide: the internal tools, the store, the listener, the
workflows.

When a configuration needs a feature the binary was built without,
validation refuses it and names the feature: an A2A listener, a cron
schedule, a CEL expression, a `mock:` endpoint, a metrics address,
`security.exec.enabled: true`, and the rest in [Build features and
crates](features.md).

```text
{"event":"config.invalid","msg":"a2a.listen needs the a2a build feature, which this binary was built without — use a release binary, or build with --features a2a"}
```

## Next steps

- [Quickstart](getting-started.md): run a first agent against a real model.
- [Your first workflow](first-workflow.md): a durable workflow you can kill
  and resume.
- [Build features and crates](features.md): every feature, and what the
  release binaries carry.
- [Deploy](deployment.md): systemd, containers and Kubernetes.
