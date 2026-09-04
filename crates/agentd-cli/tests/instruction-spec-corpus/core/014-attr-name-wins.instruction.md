:::!workflow{name=renamed}
name: original
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::
