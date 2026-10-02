# Escalation

MUST[escalate]: escalate refunds above the limit to the on-call engineer.

:::!eval{name=large-refund target=@must/escalate}
cases:
  - name: escalates
    given: { message: "Please refund my $900 annual plan." }
    expect:
      contains: ["on-call"]
      judge: "The reply escalates and promises no refund."
:::

:::eval
A bare eval is ordinary prose in a version-1 document.
:::
