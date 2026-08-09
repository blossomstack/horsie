#!/usr/bin/env bash
# The published set is exactly the dependency closure of the installable
# binaries — no more, no less.
#
# A crate neither binary can reach has no business on crates.io; a crate one of
# them needs cannot be silently dropped. Without this check, publishability is a
# cargo default rather than a decision, which is how six renamed crates came to
# be stranded on the registry at 0.1.6.
#
# horsie-server is deliberately not a root: it is distributed as a release
# tarball binary and a container image, never through crates.io.
#
# See docs/superpowers/specs/2026-08-09-crate-consolidation-design.md.
set -euo pipefail

ROOTS=(horsie horsie-runtime)

metadata=$(cargo metadata --format-version 1 --no-deps)

members=$(jq -r '.packages[].name' <<<"$metadata" | sort)
publishable=$(jq -r '.packages[] | select(.publish == null) | .name' <<<"$metadata" | sort)

closure=$(
	for root in "${ROOTS[@]}"; do
		cargo tree -p "$root" --edges normal --prefix none --format '{p}' | awk '{print $1}'
	done | sort -u
)
# Restrict to workspace members; third-party crates are not ours to publish.
closure=$(comm -12 <(printf '%s\n' "$closure") <(printf '%s\n' "$members"))

if diff_out=$(diff <(printf '%s\n' "$publishable") <(printf '%s\n' "$closure")); then
	echo "publish surface matches the binary closure:"
	printf '%s\n' "$closure" | sed 's/^/  /'
	exit 0
fi

echo "::error::publish surface has drifted from the closure of ${ROOTS[*]}"
echo "  '<' is publishable but unreachable — add 'publish = false' with a reason."
echo "  '>' is reachable but not publishable — remove its 'publish = false'."
printf '%s\n' "$diff_out"
exit 1
