## !workflow nightly

Posts a summary at 02:00.

```yaml
steps:
  wake: { kind: schedule, cron: "0 2 * * *" }
  f: { kind: finish, depends_on: [wake] }
```

::!mcp{name=after endpoint=https://mcp.example/after}
