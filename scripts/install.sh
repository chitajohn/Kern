#!/bin/sh
# Kern installer.
#
#   curl -fsSL https://raw.githubusercontent.com/chitajohn/Kern/main/scripts/install.sh | sh
#
# Environment:
#   KERN_VERSION      version to install (default: latest release, e.g. "0.1.0")
#   KERN_INSTALL_DIR  install directory (default: $HOME/.local/bin)
#   KERN_REPO         owner/repo override (default: chitajohn/Kern)
#   KERN_BASE_URL     release host, for mirrors (default: https://github.com/$KERN_REPO/releases)
set -eu

REPO="${KERN_REPO:-chitajohn/Kern}"
BIN="kern"
VERSION="${KERN_VERSION:-}"
INSTALL_DIR="${KERN_INSTALL_DIR:-${HOME}/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but was not found in PATH"
}

need uname
need tar
if command -v curl >/dev/null 2>&1; then
    download() { curl -fsSL "$1" -o "$2"; }
    download_stdout() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    download() { wget -qO "$2" "$1"; }
    download_stdout() { wget -qO- "$1"; }
else
    die "curl or wget is required"
fi

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *)
        die "unsupported OS '$os'. On Windows use scripts/install.ps1, or 'cargo install --git https://github.com/${REPO} kern-runtime'."
        ;;
esac

case "$arch" in
    x86_64|amd64)   arch_part="x86_64" ;;
    arm64|aarch64)  arch_part="aarch64" ;;
    *)
        die "unsupported architecture '$arch'. Prebuilt binaries cover x86_64 and aarch64; otherwise use 'cargo install --git https://github.com/${REPO} kern-runtime'."
        ;;
esac

TARGET="${arch_part}-${os_part}"

if [ -z "$VERSION" ]; then
    tag="$(download_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\{0,1\}\([^"]*\)".*/\1/p' \
        | head -n 1)"
    [ -n "$tag" ] || die "could not determine the latest release of ${REPO}; set KERN_VERSION explicitly"
    VERSION="$tag"
fi
VERSION="${VERSION#v}"

BASE_URL="${KERN_BASE_URL:-https://github.com/${REPO}/releases}"
BASE_URL="${BASE_URL%/}"

NAME="${BIN}-${VERSION}-${TARGET}"
ARCHIVE="${NAME}.tar.gz"
URL="${BASE_URL}/download/v${VERSION}/${ARCHIVE}"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t kern)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

say "Downloading ${BIN} ${VERSION} for ${TARGET}..."
download "$URL" "${TMP_DIR}/${ARCHIVE}" \
    || die "download failed: ${URL}
No prebuilt binary for ${TARGET} at ${VERSION}? Try: cargo install --git https://github.com/${REPO} kern-runtime"

if download "${URL}.sha256" "${TMP_DIR}/${ARCHIVE}.sha256" 2>/dev/null; then
    expected="$(cut -d' ' -f1 < "${TMP_DIR}/${ARCHIVE}.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | cut -d' ' -f1)"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | cut -d' ' -f1)"
    else
        actual=""
    fi
    if [ -n "$expected" ] && [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
        die "checksum mismatch for ${ARCHIVE}"
    fi
fi

mkdir -p "$INSTALL_DIR" || die "cannot create ${INSTALL_DIR}"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$INSTALL_DIR" --strip-components=1 "${NAME}/${BIN}" \
    || die "failed to extract ${ARCHIVE}"
chmod +x "${INSTALL_DIR}/${BIN}"

say "Installed ${INSTALL_DIR}/${BIN}"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        "${INSTALL_DIR}/${BIN}" --version >&2 || true
        ;;
    *)
        say ""
        say "${INSTALL_DIR} is not in your PATH. Add it, then restart your shell:"
        say "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac
