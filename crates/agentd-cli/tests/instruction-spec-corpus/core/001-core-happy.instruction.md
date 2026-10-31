# Minimal complete agent

Prose before any block.

:::config{id=runtime dialect="agentd/config"}
limits: { max_runs: 4 }
streams:
  inbox: { retention: { max_events: 100 } }
tools:
  disabled: [search.dangerous_tool]
:::

:::mcp{id=search title="Search" src="https://mcp.internal.example/search"}
Search the internal index.
:::

:::workflow{id=drain dialect="instruction/workflow"}
steps:
  take: {kind: stream, stream: inbox, subject: "x.*", from: earliest}
  f:    {kind: finish, depends_on: [take]}
:::

:::skill{id=tone title="house tone" trigger="writing to customers"}
Be brief.
:::

:::context{title="Facts"}
The rate limit is 10/s.
:::

:::example{title="A greeting"}
Q: hi
A: hello
:::
