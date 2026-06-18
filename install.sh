#!/usr/bin/env bash
# looop installer — install the Rust binary and drop it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
#
# Requires a Rust toolchain (cargo). Get one at https://rustup.rs.
# By default installs the published crate from crates.io.
#
# Env vars:
#   LOOOP_INSTALL_DIR   where to install (default: $HOME/.local/bin)
#   LOOOP_REF           build this git ref/branch/tag instead of crates.io
set -euo pipefail

REPO="yusukeshib/looop"
INSTALL_DIR="${LOOOP_INSTALL_DIR:-$HOME/.local/bin}"
DEST="$INSTALL_DIR/looop"

err() { printf 'install: %s\n' "$*" >&2; }

command -v cargo >/dev/null 2>&1 || {
	err "cargo (the Rust toolchain) is required — install it from https://rustup.rs"
	exit 1
}

mkdir -p "$INSTALL_DIR"

# --root puts the binary at <root>/bin/looop, so point it one level above
# INSTALL_DIR's bin. By default install the published crate from crates.io; set
# LOOOP_REF to build a specific git branch/tag/commit instead.
if [ -n "${LOOOP_REF:-}" ]; then
	err "building looop ($LOOOP_REF) from git → $DEST"
	cargo install --git "https://github.com/${REPO}.git" --rev "$LOOOP_REF" \
		--locked --root "${INSTALL_DIR%/bin}" --force looop 2>/dev/null ||
		cargo install --git "https://github.com/${REPO}.git" --branch "$LOOOP_REF" \
			--locked --root "${INSTALL_DIR%/bin}" --force looop
else
	err "installing looop from crates.io → $DEST"
	cargo install looop --locked --root "${INSTALL_DIR%/bin}" --force
fi

err "installed: $("$DEST" version 2>/dev/null || echo looop)"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
	err "note: $INSTALL_DIR is not on your PATH — add it, e.g.:"
	err "  export PATH=\"$INSTALL_DIR:\$PATH\""
	;;
esac
