:::!runtime{name=r isolation=oci image=@image/py311 network=any}
resources: {}
:::
::!image{name=py311 digest=sha256:abcdef0123 registry=r}
