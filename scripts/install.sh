#!/usr/bin/env sh
# sglangtop installer: downloads a release binary, verifies its SHA-256, and
# installs it to ~/.local/bin (no root needed).
#
# Usage:
#   sh scripts/install.sh [VERSION]
#
# Options (environment variables):
#   SGLANGTOP_REPO      GitHub "owner/name" slug (default: yongzane00/sglangtop).
#   SGLANGTOP_PREFIX    Install directory (default: ~/.local/bin).
#   SGLANGTOP_VERSION   Tag to install, e.g. v0.1.0 (default: latest release).
#
# Design: POSIX sh, no bashisms; fails closed on any checksum mismatch.

set -eu

REPO="${SGLANGTOP_REPO:-yongzane00/sglangtop}"
PREFIX="${SGLANGTOP_PREFIX:-$HOME/.local/bin}"
VERSION="${SGLANGTOP_VERSION:-${1:-}}"

err() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || err "curl is required"
command -v tar  >/dev/null 2>&1 || err "tar is required"

# sha256 tool differs across distros.
if command -v sha256sum >/dev/null 2>&1; then
    SHA256="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA256="shasum -a 256"
else
    err "need sha256sum or shasum to verify the download"
fi

# Detect architecture; only Linux is supported.
OS="$(uname -s)"
[ "$OS" = "Linux" ] || err "sglangtop releases target Linux only (got $OS)"
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64)   TARGET="x86_64-unknown-linux-musl" ;;
    aarch64|arm64)  TARGET="aarch64-unknown-linux-musl" ;;
    *) err "unsupported architecture: $ARCH" ;;
esac

# Resolve the version tag.
if [ -z "$VERSION" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | cut -d '"' -f 4) || true
    [ -n "$VERSION" ] || err "could not determine the latest release of $REPO"
fi

ARCHIVE="sglangtop-$VERSION-$TARGET.tar.gz"
BASE="https://github.com/$REPO/releases/download/$VERSION"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

echo "Downloading $ARCHIVE ..."
curl -fsSL -o "$TMP/$ARCHIVE" "$BASE/$ARCHIVE" \
    || err "download failed: $BASE/$ARCHIVE"
curl -fsSL -o "$TMP/$ARCHIVE.sha256" "$BASE/$ARCHIVE.sha256" \
    || err "checksum download failed: $BASE/$ARCHIVE.sha256"

echo "Verifying SHA-256 ..."
(
    cd "$TMP"
    # The checksum file may contain "HASH  filename" or just "HASH".
    EXPECTED=$(cut -d ' ' -f 1 < "$ARCHIVE.sha256")
    ACTUAL=$($SHA256 "$ARCHIVE" | cut -d ' ' -f 1)
    [ -n "$EXPECTED" ] || exit 1
    [ "$EXPECTED" = "$ACTUAL" ] || {
        printf 'checksum mismatch!\n  expected: %s\n  actual:   %s\n' \
            "$EXPECTED" "$ACTUAL" >&2
        exit 1
    }
) || err "SHA-256 verification failed; refusing to install"

tar -xzf "$TMP/$ARCHIVE" -C "$TMP" sglangtop
mkdir -p "$PREFIX"
install -m 755 "$TMP/sglangtop" "$PREFIX/sglangtop"

echo "Installed $("$PREFIX/sglangtop" --version) to $PREFIX/sglangtop"
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) echo "note: $PREFIX is not on your PATH" ;;
esac
