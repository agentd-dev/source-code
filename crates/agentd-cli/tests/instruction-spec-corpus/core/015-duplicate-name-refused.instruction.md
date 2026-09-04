:::!workflow{name=dup}
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::

:::!workflow{name=dup}
steps:
  s2: {kind: once}
  f2: {kind: finish, depends_on: [s2]}
:::
