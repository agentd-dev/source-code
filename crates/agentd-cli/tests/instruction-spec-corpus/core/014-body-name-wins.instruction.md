:::workflow{id=block dialect="instruction/workflow"}
name: renamed
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::

:::workflow{id=plain dialect="instruction/workflow"}
name: plain
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::
