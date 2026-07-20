#!/bin/sh
# Installs the `horsie` CLI: detects OS/arch, downloads the matching release
# tarball from the latest GitHub release, and extracts `horsie` and its
# sibling `horsie-runtime` (skipping `horsie-server`, which is server-side
# only) into ~/.local/bin. The CLI subcommand `horsie connect` spawns
# horsie-runtime from the same directory, so both must be installed together.
#
# Usage: curl -fsSL https://get.horsie.dev | sh
set -eu

REPO="blossomstack/horsie"
BINDIR="${BINDIR:-$HOME/.local/bin}"

os() {
  case "$(uname -s)" in
    Linux) echo "unknown-linux-gnu" ;;
    Darwin) echo "apple-darwin" ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
  esac
}

arch() {
  case "$(uname -m)" in
    x86_64|amd64) echo "x86_64" ;;
    arm64|aarch64) echo "aarch64" ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
}

target="$(arch)-$(os)"
latest_tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | \
  grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
if [ -z "$latest_tag" ]; then
  echo "could not determine the latest release of ${REPO}" >&2
  exit 1
fi

url="https://github.com/${REPO}/releases/download/${latest_tag}/horsie-${latest_tag}-${target}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading ${url}"
curl -fsSL "$url" -o "$tmp/horsie.tar.gz"
tar -xzf "$tmp/horsie.tar.gz" -C "$tmp" horsie horsie-runtime

mkdir -p "$BINDIR"
install -m 0755 "$tmp/horsie" "$BINDIR/horsie"
install -m 0755 "$tmp/horsie-runtime" "$BINDIR/horsie-runtime"
echo "installed horsie and horsie-runtime to ${BINDIR}"

case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo "note: ${BINDIR} is not on your PATH — add it, e.g. export PATH=\"${BINDIR}:\$PATH\"" ;;
esac
