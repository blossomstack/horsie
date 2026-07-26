# Tool output quality fixes (issues #47–#50)

Date: 2026-07-26
Status: approved
Issues: blossomstack/horsie #47, #48, #49, #50

## Background

Audit of homelab session `38643cef-5b4e-4991-af20-0c80cd9133fa` (231 messages,
kimi-code) found the runtime's tool outputs driving most of the session's waste:

- `read_file` produced 62% of all tool output; the same files were re-read up to
  9× because edit tools return bare one-line confirmations, forcing full-file
  re-reads to re-anchor after every edit (#47).
- `find_and_replace` no-match errors carried no diagnostic (77 bytes), so the
  model burned turns reverse-engineering whitespace mismatches (#48).
- `grep` has no budget of its own (`max_results` default 1000, unbounded line
  lengths) and routinely hits the blunt 50 KB middle-cut truncation in
  `runtime/src/tools/mod.rs` (#49).
- `bash` drops all captured output on timeout (child is SIGKILLed, buffers
  discarded), runs without `pipefail` so `cmd | tail` masks failures, and the
  model is never told the runtime's platform (macOS BSD userland cost ~3 turns:
  no `timeout` command, `cat -A` illegal) (#50).

## Design

All changes are in the runtime daemon's tool implementations
(`runtime/src/tools/`), plus two small touch points: the `WorkspaceScan` wire
struct (models) and `compose_system_prompt` (workflow). One PR.

### #47 — Post-edit snippets in edit tools

After a successful write, `find_and_replace` and `replace_lines` append a
numbered snippet of the changed region so the model can verify the edit without
re-reading the file:

- Snippet format: `N→` line-number prefixes (read_file stays raw; snippets are
  the anchoring aid), ±3 lines of context around the change, capped ~1.5 KB.
- The changed region is computed by a prefix/suffix diff between old and new
  content, uniform across literal/regex/replace_all and line-range edits.
- `write_file`: confirmation gains line/byte counts; no snippet (the model
  supplied the content).

### #48 — Actionable find_and_replace no-match errors

Literal mode with zero matches: before erroring, run a whitespace-normalized
near-miss search. Normalize each line (trim ends, collapse internal whitespace
runs), then slide a window of the `find` text's line count over the file's
lines.

- Exactly one window matches → error names the lines and shows them numbered:
  "find target not found as-is, but lines A–B match ignoring
  indentation/whitespace: … Adjust `find` to match exactly, or use
  replace_lines."
- Several windows → report the count and the first few line ranges (no dump).
- None → current error plus "(also checked ignoring whitespace; file has N
  lines)".
- Regex mode keeps the plain error (a pattern can't be normalized). Hint output
  capped ~1 KB.

### #49 — grep's own budgets

- Per-match line truncation at 500 chars.
- Byte budget of 20 KB across results; stop early and append a footer:
  "truncated after N matches (20 KB cap) — narrow with `path`, `file_pattern`,
  or a tighter `pattern`". The existing silent stop at `max_results` gains the
  same kind of notice.
- `max_results` default stays 1000 (the byte cap is the real bound). The 50 KB
  dispatch-level middle-cut remains as a backstop for all tools.

### #50 — bash: partial output on timeout, pipefail, platform awareness

- **Timeout**: spawn with piped stdout/stderr drained by reader tasks into
  buffers; on timeout, kill the child and return `Err` with "command timed out
  after Ns" plus captured partial output (tail-biased, capped ~10 KB —
  `dispatch` only truncates `Ok` results, so the cap lives here).
- **pipefail**: spawn `bash -o pipefail -c …` so any failing pipeline stage
  fails the command and surfaces as `is_error` via the existing non-zero-exit
  rendering. Accepted trade-off: intentional-SIGPIPE patterns (`cmd | head`)
  report exit 141, but the captured output is returned alongside. The bash
  tool's schema description (runtime-client) gains one line documenting
  pipeline semantics.
- **Platform**: `WorkspaceScan` gains an optional `platform` field (filled in
  `runtime/src/scan.rs` from `std::env::consts::OS` / `ARCH`; optional so an
  older runtime binary can still scan against a newer server).
  `compose_system_prompt` renders a one-line `# Environment` section: on macOS
  a BSD-userland caveat (no GNU `timeout`, `cat -A`; `sed -i` differs;
  coreutils may be g-prefixed), on Linux a plain "OS: linux (GNU coreutils)"
  line.

## Error handling

- Snippet/near-miss/hint rendering is best-effort: any internal failure falls
  back to today's confirmation/error text rather than failing the edit.
- All caps are hard limits with explicit notices, never silent drops.

## Testing

Extend the colocated `#[cfg(test)]` mods in each tool file:

- find_and_replace: snippet content + line numbers on success; single near-miss
  → line range named; multiple near-misses → count reported; zero near-misses →
  "(also checked ignoring whitespace)"; regex mode → plain error unchanged.
- replace_lines: snippet covers new lines with correct numbering; clamped
  ranges still render.
- write_file: counts present.
- grep: long line truncated at 500 chars; byte budget stops early with footer.
- bash: timeout returns partial output; pipefail surfaces a mid-pipe failure
  exit code; (existing timeout/exit tests updated).
- workflow: `compose_system_prompt` renders the `# Environment` section from a
  `WorkspaceScan` carrying `platform`.

Then `make check` and a single PR closing #47–#50.
