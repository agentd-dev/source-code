::agent{id=reviewer title="Reviewer" protocol=a2a src="https://agents.example/.well-known/agent-card.json"}

:::workflow{id=ask dialect="instruction/workflow"}
steps:
  s: {kind: manual}
  send: {kind: a2a.send, depends_on: [s], to: reviewer, parts: [{text: "Review this."}]}
  f: {kind: finish, depends_on: [send]}
:::
