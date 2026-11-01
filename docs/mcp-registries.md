# Use servers from an MCP registry

This guide shows how to take MCP servers from a registry, such as the MCP
project's registry at `https://registry.modelcontextprotocol.io`, instead of
copying each server's URL into your configuration. It is for operators who
approve servers and builders who call them from workflows and instruction
documents. After it you can approve a set of names from a registry, call one
of them from a workflow step by name, and read in the logs which version and
endpoint each run used.

A registry is a directory of `server.json` records: a namespaced name
(`io.github.github/github-mcp-server`), a version, and the endpoints the
server is reached at. agentd reads a record when a server is first used,
dials the endpoint it names, and never treats the registry as an authority:
your configuration decides which names may be used and how far their tools
are trusted.

## Approve servers from a registry

Declare the registry under `mcp.registries` and list the names you approve
in `servers`. A missing or empty `servers` approves nothing.

```yaml
agent:
  name: issue-desk
  instruction: "Summarize the open issues in acme/webapp."

intelligence:
  endpoints: https://api.openai.com/v1/chat/completions
  token: "{{secret:OPENAI_API_KEY}}"
  models:
    smart: {model: gpt-5.1}
  model: smart

mcp:
  registries:
    - name: official
      url: https://registry.modelcontextprotocol.io
      servers: ["io.github.github/*", "com.deepwiki/*"]
      tags: {"*": [untrusted_input]}
  servers:
    - name: github
      registry: official
      server: io.github.github/github-mcp-server
      version: latest
      headers: {Authorization: "Bearer {{secret:GITHUB_TOKEN}}"}
      allow: ["get_*", "list_*"]
```

- **`servers`** are globs over registry names. A name is
  `namespace/server`; `*` matches within one segment and never across the
  `/`, so `io.github.github/*` approves every server in that namespace and
  nothing else. `*` alone approves every name.
- **`tags`** are the Rule-of-Two legs every server from the registry counts
  as. The default is `{"*": [untrusted_input]}`. `trusted` is refused on a
  registry: vouch for one server by giving it an `mcp.servers` entry of your
  own with its own `tags`.
- **`allow`** and **`exclude`** on the registry are the widest tool surface
  any server from it offers. A server's own `allow` narrows it.
- The `mcp.servers` entry names the server by `registry` + `server` in place
  of an `endpoint`. It is dialed on first use unless it says
  `connect: boot`.

Every key is in the [configuration reference](configuration.md#mcp-registries).

## Find a server

`agentd mcp registry search` lists what a registry serves:

```sh
agentd mcp registry search github
agentd mcp registry search deepwiki --registry https://registry.modelcontextprotocol.io
agentd mcp registry search github -c agentd.yaml
```

Each line is a name, its latest version and its remotes:

```text
io.github.github/github-mcp-server	2.0.3	streamable-http https://api.githubcopilot.com/mcp/
```

With `-c`, it searches the registries the configuration declares and adds
`approved` or `not approved` after each line, by their `servers` globs. A
server that publishes only packages (stdio, npm, OCI) says so: agentd cannot
dial it.

## Call a registry server from a workflow

A step names the server where it is used, with no `mcp.servers` entry:

```yaml
# fragment: under workflows:
- name: triage
  steps:
    start: {kind: manual}
    issue:
      kind: mcp.tool
      registry: official
      server: io.github.github/github-mcp-server
      tool: get_issue
      args: {owner: acme, repo: webapp, issue_number: "{{inputs.number}}"}
      depends_on: [start]
    done: {kind: finish, depends_on: [issue]}
```

- The registry's `servers` must approve the name, or the workflow is refused
  at load. `registry`, `server` and `version` cannot be templated.
- The server is dialed when the step first runs. The step waits for the
  connection durably, up to the registry's `timeout` (or
  `mcp.default_timeout`), as it waits for any `connect: on_use` server.
- Every step naming the same server and version shares one connection,
  named `<registry>/<server>` (`official/io.github.github/github-mcp-server`),
  or `<registry>/<server>@<version>` for an exact version.
- When you also declare an `mcp.servers` entry for that server, the step's
  connection takes its `auth`, `headers`, `tags` and tool ceiling.
- `mcp.resource` and `wait {on: resource}` take the same `registry`,
  `server` and `version` fields.

There is no search node: a workflow names its servers. To sign in or warm a
server up ahead of the step that needs it, call a cheap tool on it in an
early step.

### A run keeps its version

A run calls the version of a server it first called. If the registry
publishes a new version and the server is dialed again (after a restart or a
reload), a run that started on the old version keeps calling the old one on
a connection of its own, and new runs use the new one. The log says so:

```json
{"event":"mcp.registry.pinned","run":"r-1","server":"io.acme/kb","version":"1.0.0","resolved":"2.0.0","name":"official/io.acme/kb@1.0.0"}
```

## Let an instruction document name a server

A document names a registry server by the registry's URL and the server's
name:

```markdown
:::mcp{id=gh registry="https://registry.modelcontextprotocol.io" server=io.github.github/github-mcp-server tools="get_issue"}
GitHub, for issue lookups.
:::
```

- The `registry` URL must equal one of your `mcp.registries[].url`, and the
  name must pass its `servers`. Otherwise the block stays text and the load
  says why, as a `host-policy` warning.
- The block becomes your `mcp.servers` entry for that server when you have
  one, or a server of its own under the registry's tags and ceiling. Its
  `tools=` only narrows the tool list.
- A `src` beside `registry` and `server` must be one of the record's
  remotes, or the server is refused when it is resolved: the document and the
  registry disagree.
- The document's workflow steps that name its `id` call that server. It is
  dialed right after the load.
- `account` and `resolver` are not taken from a document; each is a
  `host-policy` warning.

## Let the model find servers

With `discover: true` on a registry, the model gets two tools:

| Tool | Does |
|---|---|
| `mcp.registry.search {registry, query}` | Lists the matching records whose names your `servers` approve: name, title, description, version, remote kinds. |
| `mcp.registry.connect {registry, server}` | Connects an approved name at the registry's tags. Its tools are offered once it has connected. |

`discover` is off by default. It adds no reach the approval did not: the
lethal-trifecta check at load already counts every approved name at the
registry's tags, whether or not any is connected.

## How a record is resolved

When a registry server is first dialed, agentd reads
`GET {url}/v0.1/servers/{name}/versions/{version}` and holds the record to
these rules:

| Rule | What happens |
|---|---|
| The record's `name` differs from the name asked for | refused |
| `status: deleted` | refused |
| `status: deprecated` | connects, and warns `mcp.registry.deprecated` |
| A `streamable-http` remote | dialed |
| An `sse` remote | skipped, said in `mcp.registry.resolved.skipped`: agentd's client speaks Streamable HTTP only |
| Only `packages` (stdio, npm, OCI, PyPI) | refused: agentd starts no local process |
| A remote URL that is not `https://`, or whose host resolves to a private address | refused, unless the registry sets `allow_private: true` |
| A remote header with a `{variable}` value | filled from the `mcp.servers` entry's header of that variable's name |
| A required remote header left unfilled | refused, naming it |
| A remote header marked secret with a fixed value | not sent: a registry never supplies a credential |

A refused server fails the steps that name it with the reason, and its
record is read again after `refresh`. The record is kept in the store and
reused for `refresh` (default `1h`), across restarts.

A server that needs authorization goes through its `auth` block, as any
`mcp.servers` entry does. See [Connect MCP servers](connect-mcp.md#authenticate).

## Closed egress

Under `security.egress: closed`, a registry entry declares both the
registry's API host and the servers it resolves: a resolved remote is
admitted because the registry entry approved its name. Nothing else changes.

## Next steps

- [Connect MCP servers](connect-mcp.md): the `mcp.servers` entry, auth and
  tool ceilings.
- [Configuration reference](configuration.md#mcp-registries): every
  `mcp.registries` key.
- [Security model](security.md#mcp-registries): why a registry is a
  directory, not an authority.
- [Telemetry reference](telemetry-reference.md): the `mcp.registry.*`
  events.
