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

# On Linux, pick musl when the local glibc is too old for the -gnu build (or
# when there is no glibc at all, as on Alpine). The -gnu binaries need glibc
# >= 2.38, which rules out Debian 12 and Ubuntu 22.04 -- both extremely common
# container bases. Getting this wrong is not a download error: the binary
# installs cleanly and then dies with `GLIBC_2.38 not found` at first use, far
# from its cause.
linux_libc() {
  have=""
  if command -v ldd >/dev/null 2>&1; then
    have="$(ldd --version 2>&1 | head -1 | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+$' | tail -1)"
  fi
  # No detectable glibc version means musl, uclibc, or an ldd that does not
  # report one. Static binaries run in all three cases; -gnu does not.
  [ -n "$have" ] || { echo "musl"; return; }
  if [ "$(printf '%s\n2.38\n' "$have" | sort -V | head -1)" = "2.38" ]; then
    echo "gnu"
  else
    echo "musl"
  fi
}

os() {
  case "$(uname -s)" in
    Linux) echo "unknown-linux-$(linux_libc)" ;;
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
