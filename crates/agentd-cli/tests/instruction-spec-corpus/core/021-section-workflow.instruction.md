## !workflow drain

Drains the queue on demand; reads only.

```yaml
steps:
  wake: { kind: schedule, cron: "0 2 * * *" }
  f: { kind: finish, depends_on: [wake] }
```
