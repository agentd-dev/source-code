# Security policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report privately through GitHub's advisory flow —
[**Report a vulnerability**](https://github.com/agentd-dev/source-code/security/advisories/new)
— which creates a private thread with the maintainers.

Useful in a report: the version or commit, the config that triggers it (with
secrets redacted), what you expected the boundary to be, and what you got
instead. A proof of concept helps; a description of the mechanism is enough to
start.

You will get an acknowledgement within a few days. If a report is confirmed, we
will agree a disclosure timeline with you and credit you in the advisory unless
you would rather stay anonymous.

## What is in scope

agentd's security posture is documented in [docs/security.md](docs/security.md).
Anything that breaks one of these boundaries is in scope:

- **Local execution.** agentd spawns no tool processes by default. `exec` is off
  at two layers (the `exec` cargo feature, then `security.exec.enabled`), and
  when on it runs argv without a shell, confined to `workdir`, allow-listed on
  `argv[0]`. An escape from that fence — shell injection, `..`/symlink escape
  out of `workdir`, or running anything not on the allow-list — is a
  vulnerability.
- **Secrets.** `{{secret:…}}` / `{{secret-file:…}}` values must never reach
  logs, telemetry, error text, the `/config` view, or a child process
  environment. A leak is a vulnerability.
- **The Rule-of-Two / trifecta check.** A config combining untrusted input,
  sensitive powers and an egress path must refuse to start without
  `--allow-trifecta`. A way to assemble that combination while the check passes
  is a vulnerability.
- **The A2A listener.** Plaintext `http://` binds are loopback-only;
  non-loopback binds require client auth. Reaching a privileged operation
  without the credential the config demands — including via the pairing-code
  exchange or the interface feed — is a vulnerability. So is a display client
  seeing state outside its principal's visibility scope.
- **Transport.** TLS verification bypass, SSRF through the HTTP client's
  redirect/host handling, or request smuggling.
- **Untrusted MCP content.** Tool descriptions, prompts and resources from an
  MCP server are untrusted input by design. A path where that content gains
  control — rather than being data the model may distrust — is a vulnerability.

## What is not in scope

- **A model doing something unwise inside the powers you granted it.** If a
  config allow-lists `bash`, the agent running arbitrary commands is the
  configuration working as specified. Prompt injection that stays within the
  granted capability set is a reason to grant less, not a vulnerability — the
  fence is the allow-list and `workdir`, not the model's judgment.
- **`--allow-trifecta` behaving as documented** once an operator sets it.
- Findings that require an attacker who already has the config file, the
  process's memory, or root on the host.
- Denial of service from limits you configured (`limits.run`, budgets) doing
  their job.

## Supported versions

Fixes land on the latest minor release. Older lines get a backport only when the
issue is severe and the fix is small.

## Hardening a deployment

[docs/security.md](docs/security.md) is the full treatment. The short version:
grant the smallest capability set that works, keep `exec` compiled out unless
you need it, keep secrets as references, bind non-loopback only with client
auth, and treat the trifecta refusal as information rather than an obstacle.
