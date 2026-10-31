# Notes

::::example{title="What a server block looks like"}
:::mcp{id=quoted src="https://mcp.example/quoted"}
An example, never a server.
:::
::::

:::workflow{id=kept dialect="instruction/workflow"}
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::
