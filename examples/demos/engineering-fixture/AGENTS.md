# payments (demo fixture)

A tiny stand-in for the payments monorepo: one package, `src/ledger`, which
settles captured card payments into one amount per merchant per day.

- Test: `go test ./...` from the repository root. No network: dependencies
  are none, and `GOPROXY=off`.
- Settlement days are calendar days in UTC, whatever the machine's zone.
