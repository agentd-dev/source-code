---
spec: "1"
id: instruction://ins_param-flags
title: Bare attribute flags
---

# Release notes writer

You write release notes for ${product} targeting ${audience}.

::param{name=product type=string source=workspace required}
::param{name=audience type=enum values="developers, operators" source=prompt default=developers description="Who reads the notes"}
::param{name=tone type=string source=prompt}

:::form{title="This release"}
Which audience? ${audience}
:::

MUST: keep every note under 80 words.
