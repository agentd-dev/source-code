# Deploy assistant

:::when{environment="production"}
Every write is visible to a customer.
:::

:::when{environment="staging"}
This is a rehearsal against copied data.
:::

:::otherwise
You are in an environment this document does not know. Do not write anything.
:::

:::unless{host="agentd"}
You are running interactively; ask before any destructive step.
:::

:::unless{host="claude-code"}
This line is dropped: the host is claude-code.
:::
