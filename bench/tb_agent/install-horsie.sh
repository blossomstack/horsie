#!/bin/sh
# Install the horsie CLI into a Terminal-Bench task container and log it in.
#
# Runs via `container.exec_run`, not through the tmux session: this is setup,
# not agent work, and driving it through a terminal only adds quoting failures
# to debug.
#
# Reads HORSIE_URL and HORSIE_TOKEN from the environment. Prints one of
# HORSIE_INSTALL_OK / HORSIE_INSTALL_FAIL as its last line, because exit codes
# get swallowed by the layers above.
set -eu

fail() { echo "install error: $*" >&2; echo HORSIE_INSTALL_FAIL; exit 1; }

[ -n "${HORSIE_URL:-}" ]   || fail "HORSIE_URL is not set"
[ -n "${HORSIE_TOKEN:-}" ] || fail "HORSIE_TOKEN is not set"

BIN_DIR=/horsie-agent/bin

# Preferred path: statically linked binaries the adapter copied in. Nothing to
# download, and no libc requirement at all -- which matters because the
# published Linux builds are glibc >= 2.38 and real task images are often much
# older (Terminal-Bench's own python-3-13 base is glibc 2.36).
if [ -x "$BIN_DIR/horsie" ] && [ -x "$BIN_DIR/horsie-runtime" ]; then
    PATH="$BIN_DIR:$PATH"
    export PATH
else
    # Fallback: download. Only works where glibc is new enough, so check first
    # and say so -- otherwise the binary installs fine and dies with a
    # `GLIBC_2.38 not found` link error at first use, far from its cause.
    if command -v ldd >/dev/null 2>&1; then
        have=$(ldd --version 2>&1 | head -1 | tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+$' | tail -1)
        if [ -n "$have" ]; then
            lowest=$(printf '%s\n2.38\n' "$have" | sort -V | head -1)
            [ "$lowest" = "2.38" ] || fail \
                "glibc $have is too old for the published horsie build (needs >= 2.38); \
supply static binaries via the adapter's binaries_dir instead"
        fi
    fi

    if ! command -v curl >/dev/null 2>&1; then
        if command -v apt-get >/dev/null 2>&1; then
            apt-get update -qq >/dev/null 2>&1 || true
            apt-get install -y -qq curl ca-certificates >/dev/null 2>&1 || true
        elif command -v apk >/dev/null 2>&1; then
            apk add --no-cache curl ca-certificates >/dev/null 2>&1 || true
        fi
    fi
    command -v curl >/dev/null 2>&1 || fail "curl is not available and could not be installed"

    curl -fsSL https://get.horsie.dev | sh >/dev/null 2>&1 || fail "installer failed"
    PATH="/root/.local/bin:$PATH"
    export PATH
fi

command -v horsie >/dev/null 2>&1 || fail "horsie not on PATH"

# `--token` is the scripted alternative to the browser device flow.
horsie auth login --server "$HORSIE_URL" --token "$HORSIE_TOKEN" >/dev/null 2>&1 \
    || fail "auth login rejected the token"

echo HORSIE_INSTALL_OK
