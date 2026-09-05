# Notes

:::rationale
A bare kind this spec has never registered is inert punctuation. The prose
inside it survives and nothing is refused — this is what keeps the format a
strict superset of prose, and it is the guarantee the prose layer rests on.
:::

:::!workflow{name=kept}
steps:
  s: {kind: once}
  f: {kind: finish, depends_on: [s]}
:::
