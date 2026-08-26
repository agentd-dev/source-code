# Registering the schemas with SchemaStore

[SchemaStore](https://www.schemastore.org) is the catalog VS Code's YAML and
JSON extensions read **by default**, along with JetBrains IDEs and
`yaml-language-server`. An entry there means an agentd config gets completion
and validation from its *filename alone* — no modeline, no `yaml.schemas`, no
per-project setup.

`schemastore-catalog-entry.json` holds the two entries to submit, ready to
paste.

## Before submitting

The catalog points at live URLs, so the schemas must be **served first** — a
catalog entry pointing at a 404 is worse than no entry, because the editor
silently falls back to nothing and the user cannot tell why. Check:

```sh
curl -fsS https://agentd.dev/schema/config.json   | head -c 80
curl -fsS https://agentd.dev/schema/workflow.json | head -c 80
```

Both are published by the `site` workflow from `web/public/schema/`, which
`scripts/gen-schemas.sh` generates from the binary.

## Submitting

1. Fork `github.com/SchemaStore/schemastore`.
2. Add both entries to `src/api/json/catalog.json`, in alphabetical order by
   `name` — the file is sorted, and a PR that breaks the ordering is bounced.
3. Do **not** vendor the schema into their repo. The `url` form keeps agentd
   the single source: a release republishes `agentd.dev/schema/…` and every
   editor picks it up, where a vendored copy would go stale silently and
   report valid configs as broken.
4. Run their validator (`npm install && npm test`) before opening the PR.

## What reviewers ask about

**`fileMatch` specificity.** The catalog is shared, so a pattern that could
collide with another project's files is the usual reason for rework. Ours are
namespaced (`.agentd.yml`, `*.agentd.yaml`, `*.agentd-workflow.yaml`) rather
than generic (`config.yaml`, `workflow.yaml`), which is the point of the odd
double extension on the workflow entry.

**Draft version.** Both schemas are Draft 2020-12, self-contained (no external
`$ref`), and validate against the metaschema. `yaml-language-server` handles
2020-12; a reviewer may still ask, so it is worth saying so up front.

**Stability.** The `versions` map pins `config-1.json` / `workflow-3.json`, so
a document keeps validating against the version it was written for while the
unversioned URL follows the current major.
