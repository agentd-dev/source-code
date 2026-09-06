# Conformance fixtures

Data only — runners live with implementations (`instruction-md test conformance/` for the
TypeScript implementation; `cargo run -p instruction-core --example dump` for the Rust one).
Layout per [proposal S5](../proposals/S5-conformance-fixtures.md):

```text
corpus/<case>/doc.md · context.json · tree.json · delivered.txt
refusals/<case>/doc.md · refusals.json   ([{ "line": n | null, "message": "…" }], Appendix B shapes)
```

Includes resolve among corpus documents by front-matter `id`. Every case here passes byte-exact
(delivered text) and structure-exact (block tree) in both implementations. License: Apache-2.0
(`LICENSE` in this directory), distinct from the CC BY 4.0 specification text.
