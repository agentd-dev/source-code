# Minimal complete agent

Prose before any block.

:::!config
limits: { max_runs: 4 }
:::

:::!stream{name=inbox}
retention: { max_events: 100 }
:::

:::!mcp{name=search}
endpoint: https://mcp.internal.example/search
:::

:::!workflow{name=drain}
steps:
  take: {kind: stream, stream: inbox, subject: "x.*", from: earliest}
  f:    {kind: finish, depends_on: [take]}
:::

:::!skill{name=tone description="house tone" when="writing to customers"}
Be brief.
:::

:::!tools
disabled: [search.dangerous_tool]
:::

:::context{title="Facts"}
The rate limit is 10/s.
:::

:::example
Q: hi
A: hello
:::
