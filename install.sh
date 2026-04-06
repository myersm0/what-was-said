#!/bin/sh
set -e

REPO="myersm0/commonplace"
BIN_DIR="${HOME}/.local/bin"

info() { printf "\033[0;34m%s\033[0m\n" "$*"; }
err()  { printf "\033[0;31m%s\033[0m\n" "$*" >&2; exit 1; }

detect_platform() {
	os="$(uname -s)"
	arch="$(uname -m)"

	case "$os" in
		Linux)  os="linux" ;;
		Darwin) os="macos" ;;
		*)      err "Unsupported OS: $os" ;;
	esac

	case "$arch" in
		x86_64|amd64)  arch="x86_64" ;;
		arm64|aarch64) arch="aarch64" ;;
		*)             err "Unsupported architecture: $arch" ;;
	esac

	echo "commonplace-${os}-${arch}"
}

main() {
	artifact="$(detect_platform)"
	info "Detected platform: ${artifact}"

	url="https://github.com/${REPO}/releases/latest/download/${artifact}.tar.gz"
	info "Downloading ${url}..."

	tmpdir="$(mktemp -d)"
	trap 'rm -rf "$tmpdir"' EXIT

	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$url" -o "${tmpdir}/commonplace.tar.gz"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "${tmpdir}/commonplace.tar.gz" "$url"
	else
		err "Neither curl nor wget found."
	fi

	tar xzf "${tmpdir}/commonplace.tar.gz" -C "$tmpdir"

	mkdir -p "$BIN_DIR"
	cp "${tmpdir}/${artifact}" "${BIN_DIR}/commonplace"
	chmod +x "${BIN_DIR}/commonplace"
	info "Installed binary to ${BIN_DIR}/commonplace"

	echo ""
	case ":$PATH:" in
		*":${BIN_DIR}:"*)
			info "Ready to use." ;;
		*)
			info "Add the following to your shell profile (.bashrc, .zshrc, etc.):"
			echo ""
			echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
			echo "" ;;
	esac
	info "Requires ollama running locally: https://ollama.com"
	info "Pull an embedding model: ollama pull nomic-embed-text"
}

main
