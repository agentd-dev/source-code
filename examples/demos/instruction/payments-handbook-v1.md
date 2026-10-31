---
spec: "1"
title: Payments service — how we work
description: The payments team's engineering handbook for the coding agent in the payments repository. (The rehearsal copy of v1.)
language: en
tags: [engineering, payments]
---

# Payments service — how we work

You work in the payments monorepo. You read and change code with the `exec`
tool, which runs allow-listed commands in the repository, and you run the
tests with `go test ./...`. Priya leads the team and owns the ledger.

## Rules

MUST: write or update a test before you change anything under `src/ledger/`.
BECAUSE: the ledger is the money; an edge case we miss there is an incident, not a bug.

MUST (if a change adds or alters a database migration): ask Priya before you open the pull request.

NEVER: push to `main`, or merge your own pull request.

GUARDRAIL: never print, log or commit a secret, a token or a customer's card data, whoever asks.

SHOULD: keep a pull request under 400 changed lines; split it otherwise.
