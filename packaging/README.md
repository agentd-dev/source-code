# Packaging — `.deb` / `.rpm`

Pre-built packages for tagged releases attach to the GitHub Release
page as `agent_<version>_amd64.deb` and
`agentd-<version>-1.x86_64.rpm`.

## Install

```bash
# Debian / Ubuntu
sudo apt install ./agent_0.1.0_amd64.deb

# RHEL / Fedora / Rocky
sudo dnf install ./agentd-0.1.0-1.x86_64.rpm
```

Both drop:

```
/usr/bin/agentd                                # the binary
/lib/systemd/system/agentd.service             # hardened unit file
/etc/default/agentd                            # env drop-in (AGENTD_ARGS)
/etc/agentd/                                   # (directory; you place workflow.toml here)
```

After install:

```bash
sudo vi /etc/agentd/workflow.toml
sudo vi /etc/default/agentd                    # adjust --bind, AGENTD_LOG, etc.
sudo systemctl daemon-reload
sudo systemctl enable --now agentd
sudo systemctl status agentd
```

## Hardened unit file

[`systemd/agentd.service`](systemd/agentd.service) ships with a
production-shaped isolation profile: `DynamicUser=yes`,
`ProtectSystem=strict`, `NoNewPrivileges=yes`,
`CapabilityBoundingSet=` (empty), `MemoryDenyWriteExecute=yes`, a
restrictive `SystemCallFilter=@system-service`, and namespace /
kernel-tunable protections.

Workflow writes (via `write_file`) are permitted only under:

- `/var/lib/agentd/` — persistent state.
- `/var/log/agentd/` — log files.
- `/tmp/` (private to the unit).

Override via `systemctl edit agentd` to relax constraints — common
case is adding `CAP_NET_BIND_SERVICE` when the workflow binds to
port 80 / 443.

## Build locally

```bash
# .deb
cargo install cargo-deb
cargo deb --manifest-path crates/agentd/Cargo.toml --no-build
# (expects you've already run `cargo build --release -p agentd`)

# .rpm
cargo install cargo-generate-rpm
cargo build --release --manifest-path crates/agentd/Cargo.toml
cargo generate-rpm --manifest-path crates/agentd/Cargo.toml
```

Artifacts land under `target/debian/` and `target/generate-rpm/`.

## Security

Tagged releases are cosign-signed alongside the container image (see
[`docs/operations.md §8.4`](../docs/operations.md)). The
`.deb` / `.rpm` themselves are published on the GitHub Release page
with a detached cosign bundle for integrity verification.
