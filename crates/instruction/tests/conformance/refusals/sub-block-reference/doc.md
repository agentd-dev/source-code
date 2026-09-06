::::!test{name=t target=@function/f}
:::case{name=c}
given: {}
:::
::::
::::!function{name=f runtime=@runtime/r}
```py
x
```
::::
:::!runtime{name=r isolation=oci image=@image/i}
resources: {}
:::
::!image{name=i digest=sha256:0123456789 registry=r}
See [[case/c]].
