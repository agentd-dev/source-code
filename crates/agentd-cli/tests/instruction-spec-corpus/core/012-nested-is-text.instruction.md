::::context{title="Wrapper"}
:::workflow{name=ghost}
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::
::::
