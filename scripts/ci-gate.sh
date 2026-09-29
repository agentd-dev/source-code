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
#   ./scripts/ci-gate.sh quick    # fmt + clippy matrix + cargo deny (no test run)
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
  "--features cel"
  "--features aauth"
  "--features a2a,hot-reload"
  "--release --features internal-mocks"
  "--features sign"
  "--features oci"
  "--features decrypt"
  "--features a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt"
  "--all-features"
)

fail=0
# A check this machine could not run. It does not fail the gate — CI runs it —
# but the verdict names it, so "GATE CLEAN" never claims a check that did not
# happen.
skipped=()
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

# ci.yml's `deny` job. It runs in quick mode too: it reads the lockfile and
# builds nothing. The version is ci.yml's own pin, read rather than copied, so
# a local run that judges with another cargo-deny says so. Without cargo-deny
# the step is recorded as skipped and the verdict names it.
step "cargo deny (advisories, bans, licences, sources)"
deny_version=$(sed -n 's/^ *CARGO_DENY_VERSION: *"\{0,1\}\([^"]*\)"\{0,1\} *$/\1/p' \
                 .github/workflows/ci.yml | head -n1)
if command -v cargo-deny >/dev/null 2>&1; then
  have=$(cargo deny --version 2>/dev/null | awk '{print $2}')
  if [ -n "$deny_version" ] && [ "$have" != "$deny_version" ]; then
    echo "  NOTE  cargo-deny $have here, $deny_version in CI — results can differ"
    echo "        (install CI's: cargo install cargo-deny --locked --version $deny_version)"
  fi
  cargo deny check || fail=1
else
  echo "  SKIPPED  cargo-deny is not installed — CI's deny job will run it"
  echo "           (install: cargo install cargo-deny --locked --version ${deny_version:-<see ci.yml>})"
  skipped+=("cargo deny")
fi

if [ "${1:-}" != "quick" ]; then
  step "test (workspace, all features)"
  cargo test --workspace --all-features --no-fail-fast || fail=1

  # A release publishes agentd-net, agentd-mcp, agentd-core and agentd-cli to
  # crates.io. `cargo publish` refuses a crate whose path dependency is not
  # itself published, and that refusal arrives at TAG time — after the binaries
  # and the container are built — where it is most expensive. Prove it now.
  step "the release's crates can be published"
  # Mirror release.yml: a version already on the index is what a release
  # SKIPS, and dry-running it compares the working tree against bytes that
  # were frozen at that version — noise, not signal. Only an unpublished
  # version is a real question, and that is exactly the state a release is in
  # once it bumps.
  #
  # The unpublished crates are dry-run TOGETHER, in one `cargo publish`: cargo
  # then resolves a dependency on a sibling that is also unpublished (cli on
  # core, core on mcp, mcp on net) from the packages it just built instead of
  # from the index, so every crate the release will upload is packaged and
  # verified here — none has to wait until its dependency is on crates.io.
  pending=()
  for c in agentd-net agentd-mcp agentd-instruction agentd-core agentd-cli; do
    v=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
        | python3 -c "import json,sys;print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='$c'))")
    if [ "$(curl -s -o /dev/null -w '%{http_code}' -A 'agentd-ci-gate' \
            "https://crates.io/api/v1/crates/$c/$v")" = "200" ]; then
      echo "  --    $c $v already on crates.io (a release skips it)"
    else
      pending+=("$c")
    fi
  done
  if [ ${#pending[@]} -gt 0 ]; then
    args=()
    for c in "${pending[@]}"; do args+=(-p "$c"); done
    if cargo publish --dry-run --allow-dirty "${args[@]}" >/tmp/ci-gate-pub.log 2>&1; then
      echo "  ok    ${pending[*]} (packaged and verified together)"
    else
      echo "  FAIL  ${pending[*]}"
      grep -m6 -E '^(error|  )' /tmp/ci-gate-pub.log | sed 's/^/        /'
      fail=1
    fi
  fi

  step "published schemas are current"
  cargo build -p agentd-cli --all-features >/dev/null 2>&1
  ./scripts/gen-schemas.sh >/dev/null
  # The paths ci.yml compares. `git status` so an uncommitted NEW file (a new
  # extension's bundle) is stale too, not only a changed one.
  stale=$(git status --porcelain -- web/public/schema web/lib/workflow-nodes.json \
            web/public/a2a web/lib/extensions.json)
  if [ -n "$stale" ]; then
    echo "$stale" | sed 's/^/        /'
    echo "  the published schemas are stale — commit the regenerated files"
    fail=1
  fi
fi

verdict=$([ $fail -eq 0 ] && echo 'GATE CLEAN' || echo 'GATE FAILED')
if [ ${#skipped[@]} -gt 0 ]; then
  verdict="$verdict ($(IFS=,; echo "${skipped[*]}") NOT RUN)"
fi
printf '\n%s\n' "$verdict"
exit $fail
