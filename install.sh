#!/bin/sh
# greenlane installer.
#
# Detects your platform, downloads the matching release tarball from GitHub,
# verifies its checksum, and installs the binary onto your PATH.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mermoldy/greenlane/main/install.sh | sh
#
# Environment overrides:
#   VERSION   release tag to install (default: latest)
#   BIN_DIR   install destination   (default: /usr/local/bin)

set -eu

REPO="mermoldy/greenlane"
BIN="greenlane"
VERSION="${VERSION:-latest}"
BIN_DIR="${BIN_DIR:-/usr/local/bin}"

err() {
	echo "greenlane-install: $*" >&2
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

# Resolve the release artifact for this platform. Only the combinations that
# are actually published get a name; everything else is an explicit error.
detect_artifact() {
	os="$(uname -s)"
	arch="$(uname -m)"
	case "$os/$arch" in
	Darwin/arm64 | Darwin/aarch64) echo "${BIN}-darwin_arm64" ;;
	Linux/x86_64 | Linux/amd64) echo "${BIN}-linux_amd64" ;;
	Linux/aarch64 | Linux/arm64) echo "${BIN}-linux_arm64" ;;
	Darwin/x86_64)
		err "no prebuilt binary for Intel macOS; build from source: https://github.com/${REPO}#build-from-source"
		;;
	*)
		err "unsupported platform: $os/$arch (build from source: https://github.com/${REPO}#build-from-source)"
		;;
	esac
}

# Verify $1 (the tarball) against $1.sha256, using whichever checksum tool
# exists. The .sha256 file names the tarball, so run from its directory.
verify_checksum() {
	file="$1"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "${file}.sha256"
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 -c "${file}.sha256"
	else
		err "no checksum tool found (need sha256sum or shasum)"
	fi
}

main() {
	need uname
	need tar
	if command -v curl >/dev/null 2>&1; then
		fetch() { curl -fsSL -o "$2" "$1"; }
	elif command -v wget >/dev/null 2>&1; then
		fetch() { wget -qO "$2" "$1"; }
	else
		err "need curl or wget to download"
	fi

	name="$(detect_artifact)"
	artifact="${name}.tar.gz"

	if [ "$VERSION" = "latest" ]; then
		base="https://github.com/${REPO}/releases/latest/download"
	else
		base="https://github.com/${REPO}/releases/download/${VERSION}"
	fi

	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT

	echo "greenlane-install: downloading $artifact ($VERSION)"
	fetch "${base}/${artifact}" "${tmp}/${artifact}"
	fetch "${base}/${artifact}.sha256" "${tmp}/${artifact}.sha256"

	echo "greenlane-install: verifying checksum"
	(cd "$tmp" && verify_checksum "$artifact")

	echo "greenlane-install: extracting"
	tar -xzf "${tmp}/${artifact}" -C "$tmp"

	# Tarball layout is <name>/greenlane; fall back to a flat layout just in case.
	src="${tmp}/${name}/${BIN}"
	[ -f "$src" ] || src="${tmp}/${BIN}"
	[ -f "$src" ] || err "binary not found in archive"

	# Use sudo only when the destination isn't writable by the current user.
	if [ -w "$BIN_DIR" ] || { [ ! -e "$BIN_DIR" ] && mkdir -p "$BIN_DIR" 2>/dev/null; }; then
		install -m 755 "$src" "${BIN_DIR}/${BIN}"
	else
		echo "greenlane-install: ${BIN_DIR} not writable, using sudo"
		sudo install -m 755 "$src" "${BIN_DIR}/${BIN}"
	fi

	echo "greenlane-install: installed ${BIN} to ${BIN_DIR}/${BIN}"
	if ! command -v "$BIN" >/dev/null 2>&1; then
		echo "greenlane-install: note: ${BIN_DIR} is not on your PATH"
	fi
}

main "$@"
