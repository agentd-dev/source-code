:::!runtime{name=py}
image: ghcr.io/x@sha256:abc
:::

::::!function{name=lint runtime="@runtime/py"}
doc: lint a diff
::::

::::!test{name=lint-works target="@function/lint"}
:::case{name=one}
given: {diff: "+TODO"}
expect: {count: 1}
:::
::::
