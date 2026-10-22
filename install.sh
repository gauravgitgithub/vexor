#!/bin/sh
# Install the vexor binary without npm or Node.js.
#
#   curl -fsSL https://raw.githubusercontent.com/gauravgitgithub/vexor/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- v0.1.0        # a specific release
#
# Downloads vexor-<target>.tar.gz and SHA256SUMS from the GitHub release,
# refuses to install unless the checksum matches, and installs to
# $VEXOR_INSTALL_DIR (default: ~/.local/bin). Never uses sudo.
#
# Environment:
#   VEXOR_INSTALL_DIR    where to put the binary
#   VEXOR_DOWNLOAD_BASE  alternative base URL (mirrors, tests); release
#                        files are fetched from "$VEXOR_DOWNLOAD_BASE/<file>"

set -eu

REPO="gauravgitgithub/vexor"
VERSION="${1:-latest}"
DEST="${VEXOR_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'vexor install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

case "$(uname -s)" in
  Linux) os=unknown-linux-musl ;;
  Darwin) os=apple-darwin ;;
  *) die "unsupported OS $(uname -s); supported: Linux, macOS" ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "unsupported CPU $(uname -m); supported: x86_64, arm64" ;;
esac
target="$arch-$os"
asset="vexor-$target.tar.gz"

if [ -n "${VEXOR_DOWNLOAD_BASE:-}" ]; then
  base="$VEXOR_DOWNLOAD_BASE"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --proto '=https,file' --retry 2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify the download"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

say "downloading $asset ($VERSION)"
fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

expected=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
actual=$(sha256 "$tmp/$asset")
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual); not installing"
say "checksum ok"

tar -xzf "$tmp/$asset" -C "$tmp" vexor || die "unexpected archive contents"
mkdir -p "$DEST"
# Replace atomically, so a running vexor keeps its old binary.
cp "$tmp/vexor" "$DEST/.vexor.new"
chmod 755 "$DEST/.vexor.new"
mv -f "$DEST/.vexor.new" "$DEST/vexor"

say "installed $("$DEST/vexor" --version) to $DEST/vexor"
case ":$PATH:" in
  *":$DEST:"*) ;;
  *) say "note: $DEST is not on your PATH; add it, or run $DEST/vexor" ;;
esac
