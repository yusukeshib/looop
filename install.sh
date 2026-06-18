#!/bin/bash
# looop installer — download the prebuilt binary and drop it on your PATH.
# No Rust toolchain required.
#
#   curl -fsSL https://raw.githubusercontent.com/yusukeshib/looop/main/install.sh | bash
#
# Env vars:
#   LOOOP_INSTALL_DIR / INSTALL_DIR   where to install (default: $HOME/.local/bin)
set -e

REPO="yusukeshib/looop"
INSTALL_DIR="${LOOOP_INSTALL_DIR:-${INSTALL_DIR:-$HOME/.local/bin}}"

detect_platform() {
	local os arch
	os="$(uname -s)"
	arch="$(uname -m)"

	case "$os" in
	Linux) os="linux" ;;
	Darwin) os="darwin" ;;
	*)
		echo "Error: Unsupported OS: $os" >&2
		return 1
		;;
	esac

	case "$arch" in
	x86_64 | amd64) arch="x86_64" ;;
	aarch64 | arm64) arch="aarch64" ;;
	*)
		echo "Error: Unsupported architecture: $arch" >&2
		return 1
		;;
	esac

	echo "looop-${arch}-${os}"
}

get_latest_tag() {
	curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" |
		grep '"tag_name"' |
		sed 's/.*"tag_name": *"\([^"]*\)".*/\1/'
}

install_binary() {
	local asset tag url tmpdir
	asset="$(detect_platform)" || return 1
	echo "Detected platform: ${asset}"

	echo "Fetching latest release..."
	tag="$(get_latest_tag)"
	if [ -z "$tag" ]; then
		echo "Error: Could not determine latest release" >&2
		return 1
	fi
	echo "Latest release: ${tag}"

	url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
	tmpdir="$(mktemp -d)"
	trap 'rm -rf "$tmpdir"' EXIT

	echo "Downloading ${url}..."
	if ! curl -fsSL -o "${tmpdir}/looop" "$url"; then
		echo "Binary download failed" >&2
		return 1
	fi

	chmod +x "${tmpdir}/looop"
	mkdir -p "$INSTALL_DIR"
	mv "${tmpdir}/looop" "${INSTALL_DIR}/looop"
	echo "Installed looop to ${INSTALL_DIR}/looop"
}

install_cargo() {
	command -v cargo >/dev/null 2>&1 || return 1
	echo "Installing looop via cargo..."
	cargo install looop --locked
}

install_nix() {
	command -v nix >/dev/null 2>&1 || return 1
	echo "Installing looop via nix..."
	nix profile install "github:${REPO}"
}

echo "Installing looop..."

if install_binary; then
	echo ""
	echo "Done!"
	if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
		echo "Make sure ${INSTALL_DIR} is in your PATH:"
		echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
	fi
elif install_cargo; then
	echo ""
	echo "Done! Make sure ~/.cargo/bin is in your PATH:"
	echo "  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
elif install_nix; then
	echo ""
	echo "Done!"
else
	echo ""
	echo "Error: Could not install looop." >&2
	echo "Install one of the following and try again:" >&2
	echo "  - cargo: https://rustup.rs/" >&2
	echo "  - nix:   https://nixos.org/download/" >&2
	exit 1
fi

echo ""
echo "Get started:"
echo "  looop help        # the full design manual"
echo "  looop             # start the pulse (first run seeds + an interactive setup)"
echo ""
