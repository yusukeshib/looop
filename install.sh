#!/usr/bin/env bash
# loop installer — fetch the single `loop` script and drop it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/yusukeshib/loop/main/install.sh | bash
#
# Env vars:
#   LOOP_INSTALL_DIR   where to install (default: $HOME/.local/bin)
#   LOOP_REF           git ref/branch/tag to fetch (default: main)
set -euo pipefail

REPO="yusukeshib/loop"
REF="${LOOP_REF:-main}"
INSTALL_DIR="${LOOP_INSTALL_DIR:-$HOME/.local/bin}"
SRC_URL="https://raw.githubusercontent.com/${REPO}/${REF}/loop"
DEST="$INSTALL_DIR/loop"

err() { printf 'install: %s\n' "$*" >&2; }

command -v curl >/dev/null 2>&1 || {
	err "curl is required"
	exit 1
}

mkdir -p "$INSTALL_DIR"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

err "downloading loop ($REF) → $DEST"
curl -fsSL "$SRC_URL" -o "$tmp"

# sanity check: must look like the loop script
head -1 "$tmp" | grep -q '^#!/usr/bin/env bash' || {
	err "downloaded file does not look like loop"
	exit 1
}

chmod +x "$tmp"
mv "$tmp" "$DEST"
trap - EXIT

err "installed: $("$DEST" version 2>/dev/null || echo loop)"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	err "note: $INSTALL_DIR is not on your PATH — add it, e.g.:"
	err "  export PATH=\"$INSTALL_DIR:\$PATH\""
	;;
esac
