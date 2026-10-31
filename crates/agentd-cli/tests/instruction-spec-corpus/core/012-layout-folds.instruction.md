# Two views

::::tabs
:::tab{title="Nightly"}
The nightly run.
:::
::::

::::section{title="Machinery"}
:::workflow{id=nightly dialect="instruction/workflow"}
steps:
  wake: { kind: manual }
  f: { kind: finish, depends_on: [wake] }
:::
::::
