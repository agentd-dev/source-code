#!/bin/sh
# agentd installer — https://agentd.dev/install.sh
#
#   curl -fsSL https://agentd.dev/install.sh | sh
#
# Detects OS + architecture, downloads the matching binary from the
# latest GitHub release (or $AGENTD_VERSION), verifies it runs, and
# installs to /usr/local/bin (or ~/.local/bin without root).
# No sudo is invoked on your behalf.
#
# Options (env vars):
#   AGENTD_VERSION=v1.0.0     pin a release instead of latest
#   AGENTD_INSTALL_DIR=/path  override the install directory

set -eu

REPO="agentd-dev/source-code"
API="https://api.github.com/repos/${REPO}/releases"

say()  { printf '\033[1magentd\033[0m %s\n' "$*"; }
fail() { printf '\033[1magentd\033[0m error: %s\n' "$*" >&2; exit 1; }

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

# --- platform detection ----------------------------------------------------
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;; # static — runs everywhere
      *) fail "unsupported Linux arch '$ARCH' (x86_64 only today; build from source: cargo build --release -p agentd)" ;;
    esac ;;
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      *) fail "unsupported macOS arch '$ARCH' (Apple Silicon only today; build from source: cargo build --release -p agentd)" ;;
    esac ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    fail "on Windows, grab agentd-<version>-x86_64-pc-windows-msvc.zip from https://github.com/${REPO}/releases" ;;
  *)
    fail "unsupported OS '$OS'; build from source: cargo build --release -p agentd" ;;
esac

# --- resolve version --------------------------------------------------------
if [ "${AGENTD_VERSION:-}" ]; then
  VERSION="$AGENTD_VERSION"
else
  VERSION=$(fetch "${API}/latest" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  [ "$VERSION" ] || fail "could not resolve the latest release tag"
fi

ASSET="agentd-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

# --- download + unpack -------------------------------------------------------
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "downloading ${ASSET} ..."
fetch_to "$URL" "$TMP/$ASSET" || fail "download failed: $URL"
tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -x "$TMP/agentd" ] || fail "archive did not contain an executable 'agentd'"

# Smoke-check the binary actually runs on this machine.
"$TMP/agentd" --version >/dev/null 2>&1 || fail "downloaded binary failed to execute"

# --- install -----------------------------------------------------------------
DIR="${AGENTD_INSTALL_DIR:-}"
if [ -z "$DIR" ]; then
  if [ -w /usr/local/bin ]; then
    DIR=/usr/local/bin
  else
    DIR="$HOME/.local/bin"
    mkdir -p "$DIR"
  fi
fi

install -m 0755 "$TMP/agentd" "$DIR/agentd" 2>/dev/null || {
  cp "$TMP/agentd" "$DIR/agentd" && chmod 0755 "$DIR/agentd"
}

say "installed $("$DIR/agentd" --version | head -1) to ${DIR}/agentd"

case ":$PATH:" in
  *":$DIR:"*) : ;;
  *) say "note: ${DIR} is not on your PATH — add: export PATH=\"${DIR}:\$PATH\"" ;;
esac

say "next: agentd --help · docs: https://agentd-dev.github.io/source-code/"
