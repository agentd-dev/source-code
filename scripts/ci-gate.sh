#!/usr/bin/env bash
# Run what CI runs, before pushing.
#
# The union (`--all-features`) is NOT a substitute for the per-feature matrix:
# a feature-gated item whose only user is also feature-gated is live under the
# union and DEAD in a row that omits the feature, so `-D warnings` fails there
# and only there. That gap has produced two red builds on main; this script
# closes it.
#
#   ./scripts/ci-gate.sh          # everything
#   ./scripts/ci-gate.sh quick    # fmt + clippy matrix only (no test run)
set -uo pipefail
cd "$(dirname "$0")/.."

# The matrix from .github/workflows/ci.yml — keep in step with it.
ROWS=(
  ""                                  # default: tls + the official MCP SDK
  "--features a2a"
  "--features cron"
  "--features metrics"
  "--features otel"
  "--features oauth"
  "--features hot-reload"
  "--features config-watch"
  "--features workflow"
  "--features cel"
  "--features aauth"
  "--features a2a,hot-reload"
  "--release --features internal-mocks"
  "--features a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel"
  "--all-features"
)

fail=0
step() { printf '\n=== %s\n' "$1"; }

step "fmt"
cargo fmt --all --check || fail=1

step "clippy (workspace, all features)"
cargo clippy --workspace --all-targets --all-features -- -D warnings || fail=1

step "clippy matrix (${#ROWS[@]} rows x 2 crates)"
for F in "${ROWS[@]}"; do
  for P in agentd-core agentd-cli; do
    if ! cargo clippy -p "$P" --all-targets $F -- -D warnings >/tmp/ci-gate.log 2>&1; then
      echo "  FAIL  $P  ${F:-<default>}"
      grep -m3 -E '^error' /tmp/ci-gate.log | sed 's/^/        /'
      fail=1
    fi
  done
done
[ $fail -eq 0 ] && echo "  all rows clean"

if [ "${1:-}" != "quick" ]; then
  step "test (workspace, all features)"
  cargo test --workspace --all-features || fail=1

  # A release publishes agentd-net, agentd-mcp, agentd-core and agentd-cli to
  # crates.io. `cargo publish` refuses a crate whose path dependency is not
  # itself published, and that refusal arrives at TAG time — after the binaries
  # and the container are built — where it is most expensive. Prove it now.
  step "the release's crates can be published"
  for c in agentd-net agentd-mcp agentd-core agentd-cli; do
    if cargo publish -p "$c" --dry-run --allow-dirty >/tmp/ci-gate-pub.log 2>&1; then
      echo "  ok    $c"
    else
      echo "  FAIL  $c"
      grep -m3 -E '^(error|  )' /tmp/ci-gate-pub.log | sed 's/^/        /'
      fail=1
    fi
  done

  step "published schemas are current"
  cargo build -p agentd-cli --all-features >/dev/null 2>&1
  ./scripts/gen-schemas.sh >/dev/null
  git diff --exit-code -- web/public/schema >/dev/null || {
    echo "  web/public/schema is stale — commit the regenerated schemas"
    fail=1
  }
fi

printf '\n%s\n' "$([ $fail -eq 0 ] && echo 'GATE CLEAN' || echo 'GATE FAILED')"
exit $fail
