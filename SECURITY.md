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
  logs, telemetry, error text, the `config` op's view, or a child process
  environment. A leak is a vulnerability.
- **The Rule-of-Two / trifecta check.** A config combining untrusted input,
  sensitive powers and an egress path must refuse to start without
  `--allow-trifecta`. A way to assemble that combination while the check passes
  is a vulnerability.
- **The A2A listener.** Plaintext `http://` binds are loopback-only, and
  non-loopback binds require client auth. Every request is resolved to a
  principal before its body is read, and the rules are these:
  - the implicit operator exists only on a loopback bind with no credential
    mechanism, and never for a request carrying `Origin` — a browser always
    authenticates, even on a no-auth loopback daemon, and an `any` rule can
    never carry the operator role;
  - an operator-only op answers to the operator role alone, whatever a
    principal's grants say;
  - what a non-operator starts — tasks, runs, subagents, conversations — is
    reachable only by that principal (by its id) and operators, on the A2A
    path and through the model alike, and its `contextId` is its own name;
  - a failed credential counts against its source, and a throttled source's
    bearers are refused unchecked; nothing a web page can make a browser send
    without a credential is counted;
  - a device sign-in needs an operator's approval under a name, and a revoked
    session loses the streams it already opened.

  Reaching a privileged operation or another principal's work without the
  credential the config demands — through a browser, a relayed request, the
  device grant, a session, or the observation feed — is a vulnerability. So is
  a display client seeing state outside its principal's visibility scope.
- **The launcher.** `agentd tui` and `agentd ui` never pass the client
  `a2a.bearer` or a resolved config secret, by argv, environment, file or HTTP.
  The client signs in with a launch code minted in the daemon's own process —
  single-use, valid 60 seconds, bound to the client and the launched origin,
  redeemed only from a loopback peer, and delivered over an inherited pipe or,
  for the web UI, only in a URL fragment from a 0600 file or the owner's
  terminal. Every later browser sign-in is approved at the launcher's
  terminal, and no descriptor the launcher holds reaches a process the daemon
  spawns. `agentd-ui` holds no credential, and reads none from its environment
  or a URL — the page takes only the single-use launch code from its URL
  fragment, and strips it before its first request; a tab keeps its session in
  `sessionStorage`, bound to its endpoint. A code or session obtained any other way — by another user on the
  host, another web origin, or a remote peer — is a vulnerability. A process
  running as the same user is the user, and is out of scope.
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
  process's memory, the operator's user account, or root on the host.
- **The implicit operator behaving as documented**: on a loopback listener
  with no credential, every local account that presents nothing — and any
  same-host proxy that relays to it without `Origin` — is the operator. That is
  the posture the load warns about; configure a credential or a unix listener
  instead.
- Denial of service from limits you configured (`limits.run`, budgets) doing
  their job.

## Supported versions

Fixes land on the latest minor release. Older lines get a backport only when the
issue is severe and the fix is small.

## Hardening a deployment

[docs/security.md](docs/security.md) is the full treatment. The short version:
grant the smallest capability set that works, keep `exec` compiled out unless
you need it, keep secrets as references, bind non-loopback only with client
auth — off the machine, TLS plus `a2a.device_grant`, approved by an operator
credential — and treat the trifecta refusal as information rather than an
obstacle.
