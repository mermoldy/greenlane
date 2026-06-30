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
		sum="sha256sum"
	elif command -v shasum >/dev/null 2>&1; then
		sum="shasum -a 256"
	else
		err "no checksum tool found (need sha256sum or shasum)"
	fi
	# Log the digest we computed so it can be eyeballed against the release notes.
	echo "greenlane-install: sha256 $($sum "$file" | cut -d' ' -f1)"
	$sum -c "${file}.sha256"
}

# Decide which release to download for $artifact and set BASE (the download
# URL prefix). For an explicit VERSION we trust the caller. For "latest" we
# prefer the GitHub "latest" release, but a freshly tagged release exists before
# its build finishes uploading assets — so latest can 404 for minutes (or longer
# if one platform's build lagged or failed). In that case we walk the release
# list newest-first and fall back to the most recent one that actually has this
# platform's artifact, updating VERSION so the rest of the run uses it.
resolve_base() {
	if [ "$VERSION" != "latest" ]; then
		BASE="https://github.com/${REPO}/releases/download/${VERSION}"
		return 0
	fi

	if head_ok "https://github.com/${REPO}/releases/latest/download/${artifact}"; then
		BASE="https://github.com/${REPO}/releases/latest/download"
		return 0
	fi

	# Only walk back a few releases — if the artifact is missing this far back
	# it isn't a publish-in-progress race, so stop rather than dig endlessly.
	max_tries=5

	echo "greenlane-install: latest release has no ${artifact} yet (build still publishing?), looking for an earlier one" >&2
	if ! fetch "https://api.github.com/repos/${REPO}/releases?per_page=${max_tries}" "${tmp}/releases.json"; then
		err "could not query ${REPO} releases to fall back (network error or GitHub API rate limit)"
	fi
	# Release tags, newest first, as returned by the API.
	tags="$(grep -o '"tag_name": *"[^"]*"' "${tmp}/releases.json" | cut -d'"' -f4)"
	[ -n "$tags" ] || err "no releases found for ${REPO}"

	skipped=""
	tried=0
	for tag in $tags; do
		tried=$((tried + 1))
		if [ "$tried" -gt "$max_tries" ]; then
			break
		fi
		if head_ok "https://github.com/${REPO}/releases/download/${tag}/${artifact}"; then
			[ -z "$skipped" ] || echo "greenlane-install: WARNING: skipped newer release(s) with no ${artifact} yet: ${skipped}" >&2
			echo "greenlane-install: WARNING: falling back to ${tag} (newest published binary for your platform)" >&2
			VERSION="$tag"
			BASE="https://github.com/${REPO}/releases/download/${tag}"
			return 0
		fi
		skipped="${skipped:+$skipped }$tag"
	done

	err "no release has ${artifact} available in the last ${max_tries} releases (checked: ${skipped})"
}

main() {
	need uname
	need tar
	if command -v curl >/dev/null 2>&1; then
		fetch() { curl -fsSL -o "$2" "$1"; }
		# Existence probe: follow redirects, fail on 404, download nothing.
		head_ok() { curl -fsIL -o /dev/null "$1"; }
	elif command -v wget >/dev/null 2>&1; then
		fetch() { wget -qO "$2" "$1"; }
		head_ok() { wget -q --spider "$1"; }
	else
		err "need curl or wget to download"
	fi

	name="$(detect_artifact)"
	artifact="${name}.tar.gz"

	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT

	# Sets BASE, and may rewrite VERSION when falling back to an earlier release.
	resolve_base

	echo "greenlane-install: downloading $artifact ($VERSION)"
	fetch "${BASE}/${artifact}" "${tmp}/${artifact}"
	fetch "${BASE}/${artifact}.sha256" "${tmp}/${artifact}.sha256"

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
