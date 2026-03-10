# agentd

A bounded, workflow-driven agent runtime.

`agentd` executes predeclared DAG workflows: typed nodes, explicit
edges, compile-time-selected capabilities. The LLM is a bounded
reasoning step inside the graph — never the owner of control flow.
One binary, TOML in, deterministic traversal out.

Early days. The workflow model, validator, and execution engine are
landing first; triggers, tools, and the intelligence plane follow.

## License

MIT. See [LICENSE](LICENSE).
