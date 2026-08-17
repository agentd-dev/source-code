#!/bin/sh
# agentd installer — https://agentd.dev/install.sh
#
#   curl -fsSL https://agentd.dev/install.sh | sh
#   curl -fsSL https://agentd.dev/install.sh | sh -s -- --version v2.0.0
#
# Downloads the matching static binary from the latest GitHub release, verifies
# it against the release SHA256SUMS, checks that it runs, and installs it to
# /usr/local/bin (or ~/.local/bin when that is not writable). No sudo is ever
# invoked on your behalf.
#
# Options (flags, or the matching env var):
#   --version <tag>   AGENTD_VERSION      pin a release instead of the latest
#   --dir <path>      AGENTD_INSTALL_DIR  override the install directory
#   --no-verify       AGENTD_NO_VERIFY=1  skip checksum verification
#   --help
#
# Release binaries are Linux/musl (amd64 + arm64) and carry the cloud-native
# feature set. Two things are deliberately NOT in them, and need a source build:
#   * `exec`, the local command runner (docs/coding-agent.md)
#   * `cel`, the only dependency-bearing feature
# macOS and Windows have no prebuilt binary yet — this script tells you how to
# build instead of pretending otherwise.

set -eu

REPO="agentd-dev/source-code"
API="https://api.github.com/repos/${REPO}/releases"
DOCS="https://agentd.dev"

say()  { printf '\033[1magentd\033[0m %s\n' "$*"; }
warn() { printf '\033[1magentd\033[0m \033[33mwarning:\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1magentd\033[0m \033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Spelled out rather than read back from $0 — when this script arrives through
# `curl | sh` there is no file to read.
usage() {
  cat <<EOF
agentd installer — ${DOCS}/install.sh

  curl -fsSL ${DOCS}/install.sh | sh
  curl -fsSL ${DOCS}/install.sh | sh -s -- --version v2.0.0

Options (flag, or the matching env var):
  --version <tag>   AGENTD_VERSION       pin a release instead of the latest
  --dir <path>      AGENTD_INSTALL_DIR   override the install directory
  --no-verify       AGENTD_NO_VERIFY=1   skip checksum verification
  -h, --help

Release binaries are Linux/musl (amd64 + arm64). \`exec\` (the local command
runner) and \`cel\` are deliberately not compiled into them — those need a
source build. macOS and Windows have no prebuilt binary yet.
EOF
  exit 0
}

# --- options ----------------------------------------------------------------
VERSION="${AGENTD_VERSION:-${AGENT_VERSION:-}}"        # AGENT_* kept for 1.x
DIR="${AGENTD_INSTALL_DIR:-${AGENT_INSTALL_DIR:-}}"
NO_VERIFY="${AGENTD_NO_VERIFY:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || fail "--version needs a tag (e.g. v2.0.0)"; VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --dir)     [ $# -ge 2 ] || fail "--dir needs a path"; DIR="$2"; shift 2 ;;
    --dir=*)   DIR="${1#*=}"; shift ;;
    --no-verify) NO_VERIFY=1; shift ;;
    -h|--help) usage ;;
    *) fail "unknown option '$1' (try --help)" ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || fail "required tool '$1' not found"; }
need uname
need tar

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1"; }
  fetch_to() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO- "$1"; }
  fetch_to() { wget -qO "$2" "$1"; }
else
  fail "need curl or wget"
fi

# --- platform detection -----------------------------------------------------
# Only what the release actually builds. Anything else gets a build recipe
# rather than a 404 halfway through the download.
OS=$(uname -s)
ARCH=$(uname -m)
SOURCE_HINT="build from source instead:
    git clone https://github.com/${REPO} && cd source-code
    cargo build -p agentd-cli --release --features a2a
  see ${DOCS}/docs/getting-started/"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64)         TARGET="x86_64-unknown-linux-musl" ;;
      aarch64|arm64)        TARGET="aarch64-unknown-linux-musl" ;;
      *) fail "no prebuilt binary for Linux/${ARCH} (amd64 and arm64 only) — ${SOURCE_HINT}" ;;
    esac ;;
  Darwin)
    fail "no prebuilt macOS binary yet — ${SOURCE_HINT}" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    fail "no prebuilt Windows binary yet — use WSL2, or ${SOURCE_HINT}" ;;
  *)
    fail "unsupported OS '$OS' — ${SOURCE_HINT}" ;;
esac

# --- resolve version --------------------------------------------------------
if [ -z "$VERSION" ]; then
  VERSION=$(fetch "${API}/latest" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  [ "$VERSION" ] || fail "could not resolve the latest release tag; pin one with --version"
fi

ASSET="agentd-${VERSION}-${TARGET}.tar.gz"
BASE="https://github.com/${REPO}/releases/download/${VERSION}"

# --- download ---------------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "downloading ${ASSET} ..."
fetch_to "${BASE}/${ASSET}" "$TMP/$ASSET" \
  || fail "download failed: ${BASE}/${ASSET}
  (is ${VERSION} a real release? see https://github.com/${REPO}/releases)"

# --- verify -----------------------------------------------------------------
# Every release publishes an aggregate SHA256SUMS over its assets.
if [ "$NO_VERIFY" ]; then
  warn "checksum verification skipped (--no-verify)"
elif command -v sha256sum >/dev/null 2>&1; then
  SHA=$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  SHA=$(shasum -a 256 "$TMP/$ASSET" | cut -d' ' -f1)
else
  warn "no sha256sum/shasum on this machine — cannot verify the download"
fi

if [ "${SHA:-}" ]; then
  if fetch_to "${BASE}/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
    WANT=$(grep " \{1,2\}\*\{0,1\}${ASSET}\$" "$TMP/SHA256SUMS" | head -1 | cut -d' ' -f1)
    [ "$WANT" ] || fail "${ASSET} is not listed in the release SHA256SUMS"
    [ "$WANT" = "$SHA" ] || fail "checksum mismatch for ${ASSET}
  expected ${WANT}
  got      ${SHA}
  Do not use this download."
    say "checksum ok"
  else
    warn "release ${VERSION} publishes no SHA256SUMS — download unverified"
  fi
fi

# --- unpack + smoke-check ---------------------------------------------------
tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -x "$TMP/agentd" ] || fail "archive did not contain an executable 'agentd'"
"$TMP/agentd" --version >/dev/null 2>&1 || fail "the downloaded binary does not run on this machine"

# --- install ----------------------------------------------------------------
if [ -z "$DIR" ]; then
  if [ -w /usr/local/bin ]; then
    DIR=/usr/local/bin
  else
    DIR="$HOME/.local/bin"
    mkdir -p "$DIR"
  fi
fi
[ -d "$DIR" ] || fail "install directory does not exist: $DIR"
[ -w "$DIR" ] || fail "install directory is not writable: $DIR
  re-run with --dir \"\$HOME/.local/bin\", or with elevated privileges."

install -m 0755 "$TMP/agentd" "$DIR/agentd" 2>/dev/null \
  || { cp "$TMP/agentd" "$DIR/agentd" && chmod 0755 "$DIR/agentd"; }

say "installed $("$DIR/agentd" --version | head -1) to ${DIR}/agentd"

case ":$PATH:" in
  *":$DIR:"*) : ;;
  *) say "note: ${DIR} is not on your PATH — add: export PATH=\"${DIR}:\$PATH\"" ;;
esac

cat <<EOF

  next
    agentd --help                       every flag
    agentd --validate-config -c a.yaml  check a config without running it
    agentd tui -c a.yaml                run it with a terminal UI attached

  docs  ${DOCS}/docs/getting-started/
  TUI   ${DOCS}/docs/interface/   (needs agentd 2.0+ and \`interface.enabled\`)
EOF
