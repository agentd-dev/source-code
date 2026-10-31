# Two views

::::tabs
:::tab{title="Nightly"}
The nightly run.
:::
::::

::::section{title="Machinery"}
:::workflow{id=nightly dialect="instruction/workflow"}
name: nightly
steps:
  wake: { kind: manual }
  f: { kind: finish, depends_on: [wake] }
:::
::::
