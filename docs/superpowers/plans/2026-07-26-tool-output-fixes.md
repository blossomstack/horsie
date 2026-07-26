# Tool Output Fixes (#47–#50) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make horsie runtime tool outputs self-sufficient: edit tools return post-edit snippets, find_and_replace errors name near-misses, grep is byte-budgeted, bash returns partial output on timeout with pipefail, and the system prompt states the runtime platform.

**Architecture:** All execution-side changes live in `runtime/src/tools/` (the horsie-runtime daemon). A new `snippet` module holds shared changed-region/line-number rendering used by both edit tools. Platform awareness flows runtime → server via a new optional `WorkspaceScan.platform` field (fluorite schema `models/fluorite/runtime.fl`) into `compose_system_prompt`'s new `# Environment` section.

**Tech Stack:** Rust (tokio), fluorite codegen (models), cargo.

## Global Constraints

- Repo: worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie-tool-output-fixes`, branch `feat/tool-output-fixes`.
- Wire-compat: `WorkspaceScan.platform` MUST be `Option<String>` so an older runtime binary still deserializes against a newer server.
- Clippy lints in test mods: add `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]` (existing convention).
- `dispatch` in `runtime/src/tools/mod.rs` truncates only `ToolResult::Ok` streams at 50 KB — error strings must be self-capped.
- One PR closing #47, #48, #49, #50. Commit per task.

---

### Task 1: snippet module (shared edit-region rendering)

**Files:**
- Create: `runtime/src/tools/snippet.rs`
- Modify: `runtime/src/tools/mod.rs:1-8` (module list)

**Interfaces:**
- Produces: `pub fn changed_range(old: &str, new: &str) -> (usize, usize)` — 1-based inclusive line range that differs between two contents. `pub fn numbered_window(content: &str, start_line: usize, end_line: usize) -> String` — `N→ `-prefixed render with ±3 context lines, capped 1500 bytes.

- [ ] **Step 1: Write the failing test**

Create `runtime/src/tools/snippet.rs` with only the doc comment and this test mod:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn changed_range_single_line_edit() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nc\n";
        assert_eq!(changed_range(old, new), (2, 2));
    }

    #[test]
    fn changed_range_insertion_widens() {
        let old = "a\nd\n";
        let new = "a\nb\nc\nd\n";
        assert_eq!(changed_range(old, new), (2, 3));
    }

    #[test]
    fn numbered_window_has_line_numbers_and_context() {
        let content = (1..=20).map(|i| format!("line{i}\n")).collect::<String>();
        let out = numbered_window(&content, 10, 11);
        assert!(out.contains("7→ line7"), "context above: {out}");
        assert!(out.contains("10→ line10"), "changed line: {out}");
        assert!(out.contains("14→ line14"), "context below: {out}");
        assert!(!out.contains("6→ line6"), "no extra context: {out}");
    }

    #[test]
    fn numbered_window_clamps_to_file_edges() {
        let out = numbered_window("only\n", 1, 1);
        assert_eq!(out, "1→ only");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-runtime snippet`
Expected: FAIL — `changed_range`/`numbered_window` not found.

- [ ] **Step 3: Write minimal implementation**

Add to `runtime/src/tools/snippet.rs` above the test mod:

```rust
//! Shared rendering for edit-tool confirmations: locate the region an edit
//! changed and render it with line numbers, so the model can verify the result
//! without re-reading the whole file.

/// Lines of context rendered on each side of a changed region.
const CONTEXT_LINES: usize = 3;
/// Hard cap on a rendered snippet; a truncated snippet ends with a marker.
const MAX_SNIPPET_BYTES: usize = 1500;

/// 1-based inclusive line range that differs between `old` and `new`, computed
/// as the span between their common byte prefix and common byte suffix.
pub fn changed_range(old: &str, new: &str) -> (usize, usize) {
    let mut prefix = 0;
    while prefix < old.len().min(new.len()) && old.as_bytes()[prefix] == new.as_bytes()[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = 0;
    let max_suffix = old.len().min(new.len()) - prefix;
    while suffix < max_suffix
        && old.as_bytes()[old.len() - 1 - suffix] == new.as_bytes()[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    let start = new[..prefix].matches('\n').count() + 1;
    // When the change ends exactly at a line boundary, the trailing newline
    // belongs to the last changed line, not the one after it.
    let mut pos = new.len() - suffix;
    if pos > prefix && new.as_bytes()[pos - 1] == b'\n' {
        pos -= 1;
    }
    let end = new[..pos].matches('\n').count() + 1;
    (start, end.max(start))
}

/// Render `start_line..=end_line` (1-based, clamped to the file) with
/// [`CONTEXT_LINES`] of slack each side and `N→ ` prefixes. Output is capped at
/// [`MAX_SNIPPET_BYTES`]; a cut snippet ends with a truncation marker.
pub fn numbered_window(content: &str, start_line: usize, end_line: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let lo = start_line.saturating_sub(CONTEXT_LINES + 1); // 0-based
    let hi = (end_line + CONTEXT_LINES).min(lines.len()); // exclusive
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(hi).skip(lo) {
        let row = format!("{}→ {}\n", i + 1, line);
        if out.len() + row.len() > MAX_SNIPPET_BYTES {
            out.push_str("… (snippet truncated)");
            break;
        }
        out.push_str(&row);
    }
    out.trim_end().to_string()
}
```

In `runtime/src/tools/mod.rs`, add after `pub mod replace_lines;`:

```rust
pub(crate) mod snippet;
```

(Keep alphabetical placement: between `replace_lines` and `write_file`.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p horsie-runtime snippet`
Expected: 4 PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/snippet.rs runtime/src/tools/mod.rs
git commit -m "runtime: shared snippet rendering for edit-tool confirmations (#47)"
```

---

### Task 2: replace_lines returns a post-edit snippet

**Files:**
- Modify: `runtime/src/tools/replace_lines.rs`

**Interfaces:**
- Consumes: `super::snippet::{changed_range, numbered_window}` from Task 1.
- Produces: success stdout becomes `"Replaced lines {a}-{b} in '{path}'.\n\n{snippet}"`.

- [ ] **Step 1: Write the failing test**

Add to the existing test mod in `runtime/src/tools/replace_lines.rs`:

```rust
    #[tokio::test]
    async fn success_includes_numbered_snippet() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\nf\ng\n").unwrap();
        let result = exec(
            dir.path(),
            ReplaceLinesInput {
                path: "f.txt".into(),
                start_line: 3,
                end_line: 4,
                replacement: "X\nY".into(),
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("Replaced lines 3-4"), "{}", o.stdout);
                assert!(o.stdout.contains("3→ X"), "{}", o.stdout);
                assert!(o.stdout.contains("4→ Y"), "{}", o.stdout);
                assert!(o.stdout.contains("1→ a"), "context above: {}", o.stdout);
                assert!(o.stdout.contains("7→ g"), "context below: {}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-runtime replace_lines`
Expected: FAIL — stdout lacks `3→ X`.

- [ ] **Step 3: Write minimal implementation**

In `runtime/src/tools/replace_lines.rs`, change the `spawn_blocking` body to keep the old content and append the snippet (also preserve a trailing newline when the original had one — today the splice silently strips it):

```rust
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let trailing_newline = content.ends_with('\n');
        let mut lines: Vec<&str> = content.lines().collect();
        let start = (input.start_line as usize)
            .saturating_sub(1)
            .min(lines.len());
        // Clamp the end to at least `start` so an inverted range degrades to an
        // insertion rather than panicking on a reversed splice range.
        let end = (input.end_line as usize).min(lines.len()).max(start);
        let replacement_lines: Vec<&str> = input.replacement.lines().collect();
        lines.splice(start..end, replacement_lines);
        let mut new_content = lines.join("\n");
        if trailing_newline {
            new_content.push('\n');
        }
        std::fs::write(&path, &new_content).map_err(|e| e.to_string())?;
        let (s, e) = super::snippet::changed_range(&content, &new_content);
        Ok::<String, String>(format!(
            "Replaced lines {}-{} in '{}'.\n\n{}",
            input.start_line,
            input.end_line,
            input.path,
            super::snippet::numbered_window(&new_content, s, e),
        ))
    })
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p horsie-runtime replace_lines`
Expected: PASS (existing `replaces_line_range` compares file content — with the trailing-newline preservation, its input `"a\nb\nc\nd"` (no trailing newline) is unaffected).

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/replace_lines.rs
git commit -m "runtime: replace_lines returns numbered post-edit snippet (#47)"
```

---

### Task 3: find_and_replace snippet + near-miss no-match errors

**Files:**
- Modify: `runtime/src/tools/find_and_replace.rs`

**Interfaces:**
- Consumes: `super::snippet::{changed_range, numbered_window}` from Task 1.
- Produces: success stdout becomes `"Replaced {count} occurrence(s) in '{path}'.\n\n{snippet}"`; literal no-match error gains near-miss diagnostics.

- [ ] **Step 1: Write the failing tests**

Add to the existing test mod in `runtime/src/tools/find_and_replace.rs`:

```rust
    #[tokio::test]
    async fn success_includes_numbered_snippet() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "aa\nbb\ncc\ndd\n").unwrap();
        let result = exec(dir.path(), input("f.txt", "cc", "XX")).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("Replaced 1 occurrence"), "{}", o.stdout);
                assert!(o.stdout.contains("3→ XX"), "{}", o.stdout);
                assert!(o.stdout.contains("1→ aa"), "context: {}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn no_match_reports_single_near_miss() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        // `find` has wrong indentation on line 2.
        let result = exec(dir.path(), input("f.txt", "fn main() {\nlet x = 1;", "z")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("lines 1-2 match ignoring"), "{}", e.reason);
                assert!(e.reason.contains("2→     let x = 1;"), "{}", e.reason);
            }
        }
    }

    #[tokio::test]
    async fn no_match_reports_multiple_near_misses() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "foo\n  foo\nfoo\n").unwrap();
        let result = exec(dir.path(), input("f.txt", " foo", "z")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                // " foo" normalizes to "foo" — matches lines 1, 2, 3.
                assert!(e.reason.contains("3 regions match ignoring"), "{}", e.reason);
            }
        }
    }

    #[tokio::test]
    async fn no_match_without_near_miss_says_so() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello\n").unwrap();
        let result = exec(dir.path(), input("f.txt", "missing", "x")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("not found"), "{}", e.reason);
                assert!(e.reason.contains("also checked ignoring whitespace"), "{}", e.reason);
            }
        }
    }
```

Note: the existing `missing_target_is_an_error` test asserts `contains("not found")` — still satisfied.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-runtime find_and_replace`
Expected: 4 FAIL.

- [ ] **Step 3: Write minimal implementation**

In `runtime/src/tools/find_and_replace.rs`, replace the `spawn_blocking` body:

```rust
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let use_regex = input.regex.unwrap_or(false);
        let replace_all = input.replace_all.unwrap_or(false);

        let re = if use_regex {
            Some(regex::Regex::new(&input.find).map_err(|e| e.to_string())?)
        } else {
            None
        };

        // Count matches first so we can both enforce uniqueness (the safe default)
        // and report how many sites changed.
        let count = match &re {
            Some(re) => re.find_iter(&content).count(),
            None => content.matches(&input.find).count(),
        };
        if count == 0 {
            return Err(no_match_error(&input.path, &content, &input.find, use_regex));
        }
        if !replace_all && count > 1 {
            return Err(format!(
                "find target matched {count} times in '{}' — add surrounding context so it \
                 identifies exactly one location, or set replace_all to change every occurrence",
                input.path
            ));
        }

        // With the guard above, `count == 1` in the single case, so replacing all
        // matches replaces exactly that one.
        let new_content = match &re {
            Some(re) => re
                .replace_all(&content, input.replace.as_str())
                .into_owned(),
            None => content.replace(&input.find, &input.replace),
        };
        std::fs::write(&path, &new_content).map_err(|e| e.to_string())?;
        let (s, e) = super::snippet::changed_range(&content, &new_content);
        Ok::<String, String>(format!(
            "Replaced {count} occurrence(s) in '{}'.\n\n{}",
            input.path,
            super::snippet::numbered_window(&new_content, s, e),
        ))
    })
```

Add below `exec` (outside the test mod):

```rust
/// The no-match error. Literal searches get a whitespace-normalized near-miss
/// hunt — the overwhelmingly common cause is an indentation drift between the
/// model's `find` and the file. Regex searches can't be normalized and keep the
/// plain error.
fn no_match_error(path: &str, content: &str, find: &str, regex_mode: bool) -> String {
    let base = format!("find target not found in '{path}'");
    if regex_mode {
        return base;
    }
    match near_miss_lines(content, find).as_slice() {
        [] => format!(
            "{base} (also checked ignoring whitespace; file has {} lines)",
            content.lines().count()
        ),
        [(a, b)] => format!(
            "find target not found as-is, but lines {a}-{b} match ignoring \
             indentation/whitespace:\n{}\nAdjust `find` to match the file exactly, \
             or use replace_lines with that range.",
            super::snippet::numbered_window(content, *a, *b)
        ),
        many => {
            let ranges = many
                .iter()
                .take(3)
                .map(|(a, b)| format!("{a}-{b}"))
                .collect::<Vec<_>>()
                .join(", ");
            let more = if many.len() > 3 { ", …" } else { "" };
            format!(
                "find target not found; {} regions match ignoring \
                 indentation/whitespace (lines {ranges}{more}). Adjust `find` to \
                 match the file exactly.",
                many.len()
            )
        }
    }
}

/// Collapse a line to whitespace-normalized form for near-miss comparison.
fn normalize_ws(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 1-based inclusive line ranges whose whitespace-normalized content equals the
/// normalized `find` (same line count, line-by-line).
fn near_miss_lines(content: &str, find: &str) -> Vec<(usize, usize)> {
    let needle: Vec<String> = find.lines().map(normalize_ws).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<String> = content.lines().map(normalize_ws).collect();
    if hay.len() < needle.len() {
        return Vec::new();
    }
    (0..=hay.len() - needle.len())
        .filter(|&i| hay[i..i + needle.len()] == needle[..])
        .map(|i| (i + 1, i + needle.len()))
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-runtime find_and_replace`
Expected: all PASS (existing 5 + new 4).

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/find_and_replace.rs
git commit -m "runtime: find_and_replace snippet + near-miss no-match errors (#47 #48)"
```

---

### Task 4: write_file confirmation gains counts

**Files:**
- Modify: `runtime/src/tools/write_file.rs`

**Interfaces:**
- Produces: success stdout becomes `"Wrote {n} lines ({m} bytes) to '{path}'."`

- [ ] **Step 1: Write the failing test**

Add to the existing test mod in `runtime/src/tools/write_file.rs`:

```rust
    #[tokio::test]
    async fn confirmation_reports_counts() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            WriteFileInput {
                path: "out.txt".into(),
                content: "a\nb\nc\n".into(),
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("3 lines"), "{}", o.stdout);
                assert!(o.stdout.contains("6 bytes"), "{}", o.stdout);
                assert!(o.stdout.contains("out.txt"), "{}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
```

(`ToolResult` is already imported at the top of the file.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-runtime write_file`
Expected: FAIL — stdout is "File written."

- [ ] **Step 3: Write minimal implementation**

In `runtime/src/tools/write_file.rs`, change the success arm's blocking closure to return the counts (capture lengths before the move into `std::fs::write`):

```rust
    match tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let lines = input.content.lines().count();
        let bytes = input.content.len();
        std::fs::write(&path, &input.content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!(
            "Wrote {lines} lines ({bytes} bytes) to '{}'.",
            input.path
        ))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
        }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p horsie-runtime write_file`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/write_file.rs
git commit -m "runtime: write_file confirmation reports line/byte counts (#47)"
```

---

### Task 5: grep line truncation + byte budget

**Files:**
- Modify: `runtime/src/tools/grep.rs`

**Interfaces:**
- Produces: matching lines truncated at 500 chars; output stops past a 20 KB budget or at `max_results`, each with an explanatory footer line.

- [ ] **Step 1: Write the failing tests**

Add to the existing test mod in `runtime/src/tools/grep.rs`:

```rust
    #[tokio::test]
    async fn long_matching_lines_are_truncated() {
        let dir = TempDir::new().unwrap();
        let long = format!("hit{}", "x".repeat(2000));
        std::fs::write(dir.path().join("f.txt"), long).unwrap();
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "hit".into(),
                path: None,
                file_pattern: None,
                max_results: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.len() < 1000, "line not truncated: {}", o.stdout.len());
                assert!(o.stdout.contains('…'), "no truncation marker: {}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn byte_budget_stops_early_with_footer() {
        let dir = TempDir::new().unwrap();
        // 400 files x ~60-byte match lines ≈ 24 KB — past the 20 KB budget.
        for i in 0..400 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), format!("hit {:>50}\n", i)).unwrap();
        }
        let result = exec(
            dir.path(),
            GrepInput {
                pattern: "hit".into(),
                path: None,
                file_pattern: None,
                max_results: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("20 KB cap"), "no footer: {}", &o.stdout[o.stdout.len().saturating_sub(300)..]);
                assert!(o.stdout.len() < 23_000, "output not bounded: {}", o.stdout.len());
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-runtime grep`
Expected: 2 FAIL.

- [ ] **Step 3: Write minimal implementation**

In `runtime/src/tools/grep.rs`, replace the constants area and `spawn_blocking` body:

```rust
use horsie_models::runtime::{GrepInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

/// Per-line cap so a single minified or generated line can't eat the budget.
const MAX_LINE_CHARS: usize = 500;
/// Total output budget. Tool output rides the conversation history, so grep
/// stops well below the dispatcher's 50 KB saw and says so.
const MAX_OUTPUT_BYTES: usize = 20_000;

pub async fn exec(working_dir: &Path, input: GrepInput) -> ToolResult {
    let base = match &input.path {
        Some(p) => working_dir.join(p),
        None => working_dir.to_path_buf(),
    };
    let file_pat = input
        .file_pattern
        .clone()
        .unwrap_or_else(|| "**/*".to_string());
    let max = input.max_results.unwrap_or(1000) as usize;
    let pattern = input.pattern.clone();
    match tokio::task::spawn_blocking(move || {
        let re = regex::Regex::new(&pattern).map_err(|e| e.to_string())?;
        let glob_pat = format!("{}/{}", base.display(), file_pat);
        let mut results: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        // Why the scan stopped early; drives the footer.
        let mut stopped: Option<String> = None;
        'outer: for path in glob::glob(&glob_pat).map_err(|e| e.to_string())?.flatten() {
            if path.is_file()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        let line = if line.chars().count() > MAX_LINE_CHARS {
                            format!("{}…", line.chars().take(MAX_LINE_CHARS).collect::<String>())
                        } else {
                            line.to_string()
                        };
                        let row = format!("{}:{}: {}", path.display(), i + 1, line);
                        bytes += row.len() + 1;
                        results.push(row);
                        if results.len() >= max {
                            stopped = Some(format!(
                                "stopped at max_results={max} — narrow with `path`, \
                                 `file_pattern`, or a tighter `pattern`"
                            ));
                            break 'outer;
                        }
                        if bytes > MAX_OUTPUT_BYTES {
                            stopped = Some(format!(
                                "truncated after {} matches ({} KB cap) — narrow with \
                                 `path`, `file_pattern`, or a tighter `pattern`",
                                results.len(),
                                MAX_OUTPUT_BYTES / 1000
                            ));
                            break 'outer;
                        }
                    }
                }
            }
        }
        let mut out = results.join("\n");
        if let Some(reason) = stopped {
            out.push_str(&format!("\n[{reason}]"));
        }
        Ok::<String, String>(out)
    })
```

(The `Ok`/`Err` match arms after `.await` stay as they are.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-runtime grep`
Expected: PASS (existing `grep_finds_match` + 2 new).

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/grep.rs
git commit -m "runtime: grep per-line truncation and 20 KB byte budget (#49)"
```

---

### Task 6: bash — pipefail + partial output on timeout

**Files:**
- Modify: `runtime/src/tools/bash.rs`

**Interfaces:**
- Produces: unchanged `ToolResult` shape; on timeout `ToolError.reason` is `"command timed out after {n}s"` + optional `"\n--- captured output before timeout ---\n{tail}"` (tail ≤ 10 KB). Pipelines now run under `pipefail`.

- [ ] **Step 1: Write the failing tests**

Add to the existing test mod in `runtime/src/tools/bash.rs`:

```rust
    #[tokio::test]
    async fn timeout_returns_partial_output() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "echo before-timeout; sleep 5".to_string(),
                timeout_secs: Some(1),
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => panic!("expected timeout, got exit {}", o.exit_code),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("timed out"), "{}", e.reason);
                assert!(e.reason.contains("before-timeout"), "{}", e.reason);
            }
        }
    }

    #[tokio::test]
    async fn pipefail_surfaces_mid_pipe_failure() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "false | true".to_string(),
                timeout_secs: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.exit_code, 1, "pipefail should fail the pipeline"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-runtime bash`
Expected: `timeout_returns_partial_output` FAIL (no "before-timeout"), `pipefail_surfaces_mid_pipe_failure` FAIL (exit 0).

- [ ] **Step 3: Write minimal implementation**

Replace the non-test portion of `runtime/src/tools/bash.rs`:

```rust
use horsie_models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Wall-clock limit applied when the caller does not specify one. Bounds runaway
/// or hung commands (e.g. waiting on stdin) so a single tool call cannot stall the
/// agent forever.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Cap on the partial output included in a timeout error. Tail-biased: the end
/// of the log is where a hang usually shows. Error strings are not truncated by
/// the dispatcher, so the cap must live here.
const MAX_PARTIAL_BYTES: usize = 10_000;

pub async fn exec(working_dir: &Path, input: BashInput) -> ToolResult {
    let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    // pipefail: a failing stage anywhere in a pipeline fails the command, so
    // `cargo test 2>&1 | tail` can't mask a test failure behind tail's exit 0.
    let child = tokio::process::Command::new("bash")
        .arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            return ToolResult::Err(ToolError {
                reason: e.to_string(),
            });
        }
    };

    // Drain both streams as they arrive so a timeout can report what the
    // command produced before it was killed — the difference between
    // "still compiling" and "genuinely hung" for the agent.
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for (pipe, buf) in [
        (child.stdout.take(), Arc::clone(&stdout_buf)),
        (child.stderr.take(), Arc::clone(&stderr_buf)),
    ] {
        let mut pipe = pipe.expect("stdout/stderr are piped");
        readers.push(tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.lock().expect("buffer poisoned").extend_from_slice(&chunk[..n]);
            }
        }));
    }

    let drain = |buf: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&buf.lock().expect("buffer poisoned")).into_owned()
    };

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            for r in readers {
                let _ = r.await;
            }
            ToolResult::Ok(ToolOutput {
                stdout: drain(&stdout_buf),
                stderr: drain(&stderr_buf),
                exit_code: status.code().unwrap_or(-1),
            })
        }
        Ok(Err(e)) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
        Err(_elapsed) => {
            // Kill, reap, then collect whatever the readers captured.
            let _ = child.kill().await;
            let _ = child.wait().await;
            for r in readers {
                let _ = r.await;
            }
            let mut reason = format!("command timed out after {}s", timeout.as_secs());
            let captured = format!("{}{}", drain(&stdout_buf), drain(&stderr_buf));
            let tail = tail_str(&captured, MAX_PARTIAL_BYTES);
            if !tail.trim().is_empty() {
                reason.push_str(&format!("\n--- captured output before timeout ---\n{tail}"));
            }
            ToolResult::Err(ToolError { reason })
        }
    }
}

/// The last `max` bytes of `s`, nudged to a UTF-8 char boundary.
fn tail_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-runtime bash`
Expected: PASS (existing 4 + new 2).

- [ ] **Step 5: Commit**

```bash
git add runtime/src/tools/bash.rs
git commit -m "runtime: bash pipefail + partial output on timeout (#50)"
```

---

### Task 7: bash tool description documents pipeline/timeout semantics

**Files:**
- Modify: `runtime-client/src/tools/bash.rs:22-24`

- [ ] **Step 1: Update the description**

Change the `ToolSpec` description to:

```rust
            description: "Execute a bash command in the runtime's working directory. \
                Optionally set 'timeout_secs' to bound how long the command may run. \
                Pipelines run with pipefail: the command fails if any stage fails. \
                On timeout, output captured so far is returned with the error."
                .to_string(),
```

- [ ] **Step 2: Verify**

Run: `cargo test -p horsie-runtime-client bash`
Expected: PASS (spec text is not asserted; compile + existing tests).

- [ ] **Step 3: Commit**

```bash
git add runtime-client/src/tools/bash.rs
git commit -m "runtime-client: bash tool description documents pipefail/timeout (#50)"
```

---

### Task 8: WorkspaceScan.platform → `# Environment` prompt section

**Files:**
- Modify: `models/fluorite/runtime.fl:51-57` (add field)
- Modify: `runtime/src/scan.rs:49` (fill field)
- Modify: `workflow/src/workspace.rs` (WorkspaceContext.platform, interpret, compose_system_prompt, environment_section, ws_scan test helper, new test)
- Modify (test/mock literals, add `platform: None,`): `workflow/tests/workspace_context.rs:32`, `workflow/src/context.rs:579`, `runtime-client/src/client.rs:136`, `executor/src/executor.rs:832`, `executor/src/socket_transport.rs:275`, `executor-client/src/ws_transport.rs:290`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `WorkspaceContext.platform: Option<String>`; `environment_section(platform: &str) -> String` (private); `# Environment` section in composed prompts.

- [ ] **Step 1: Write the failing test**

Add to the test mod in `workflow/src/workspace.rs`:

```rust
    #[test]
    fn environment_section_renders_platform() {
        let ctx = WorkspaceContext {
            workspaces: vec![],
            platform: Some("macos-aarch64".to_string()),
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx, None).unwrap();
        assert!(prompt.contains("# Environment"), "{prompt}");
        assert!(prompt.contains("BSD userland"), "{prompt}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-workflow environment_section`
Expected: FAIL — `platform` field doesn't exist.

- [ ] **Step 3: Write minimal implementation**

In `models/fluorite/runtime.fl`, extend `WorkspaceScan`:

```
struct WorkspaceScan {
    name: String,
    path: String,
    is_git_repo: bool,
    instructions: Option<ScannedFile>,
    skills: Vec<ScannedFile>,
    /// Runtime OS/arch (`<os>-<arch>`, e.g. "macos-aarch64"); optional so an
    /// older runtime binary still deserializes against a newer server.
    platform: Option<String>,
}
```

In `runtime/src/scan.rs`, fill it in the `WorkspaceScan` construction:

```rust
            WorkspaceScan {
                name: ws.name.clone(),
                path: dir.display().to_string(),
                is_git_repo: is_git_repo(dir),
                instructions,
                skills,
                platform: Some(format!(
                    "{}-{}",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )),
            }
```

In `workflow/src/workspace.rs`:

```rust
#[derive(Clone, Default)]
pub struct WorkspaceContext {
    pub workspaces: Vec<WorkspaceInfo>,
    /// Runtime OS/arch from the scan (all workspaces share one runtime).
    pub platform: Option<String>,
}
```

```rust
fn interpret(raw: Vec<WorkspaceScan>) -> WorkspaceContext {
    let platform = raw.iter().find_map(|w| w.platform.clone());
    WorkspaceContext {
        workspaces: raw.into_iter().map(interpret_one).collect(),
        platform,
    }
}
```

In `compose_system_prompt`, insert after the agent-role push and before the `# Workspaces` block:

```rust
    if let Some(p) = &ws.platform {
        sections.push(environment_section(p));
    }
```

Add below `compose_system_prompt`:

```rust
/// The `# Environment` section: one line telling the model which OS userland
/// its bash/filesystem tools run on, so it doesn't probe by failing (BSD-vs-GNU
/// differences have burned whole turns).
fn environment_section(platform: &str) -> String {
    let os = platform.split('-').next().unwrap_or(platform);
    match os {
        "macos" => "# Environment\nOS: macOS — BSD userland: no GNU `timeout` or \
            `cat -A`; `sed -i` requires an explicit backup argument (`sed -i ''`); \
            GNU coreutils, if installed, are g-prefixed (`gtimeout`, `gsed`)."
            .to_string(),
        "linux" => "# Environment\nOS: Linux — GNU coreutils available.".to_string(),
        other => format!("# Environment\nOS: {other}."),
    }
}
```

Then fix every `WorkspaceScan { … }` literal to compile — add `platform: None,` in: `workflow/tests/workspace_context.rs:32`, `workflow/src/context.rs:579`, `runtime-client/src/client.rs:136`, `executor/src/executor.rs:832`, `executor/src/socket_transport.rs:275`, `executor-client/src/ws_transport.rs:290`, and the `ws_scan` helper at `workflow/src/workspace.rs:429`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-workflow workspace` and `cargo check --workspace`
Expected: PASS; no remaining `WorkspaceScan` literal errors anywhere.

- [ ] **Step 5: Commit**

```bash
git add models/fluorite/runtime.fl runtime/src/scan.rs workflow/ runtime-client/src/client.rs executor/ executor-client/
git commit -m "runtime: advertise platform in WorkspaceScan; prompt gains # Environment (#50)"
```

---

### Task 9: Full verification + PR

- [ ] **Step 1: Run the full check**

Run: `make check`
Expected: green (fmt, clippy, all workspace tests, TS codegen drift if configured).

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin feat/tool-output-fixes
gh pr create --title "runtime: self-sufficient tool outputs (snippets, near-miss errors, grep/bash budgets, platform)" --body "..."
```

Body: one paragraph per issue (#47 snippets/counts, #48 near-miss, #49 grep budgets, #50 bash pipefail+timeout partial+platform), `Closes #47`, `Closes #48`, `Closes #49`, `Closes #50`; no hard-wrapped lines (GitHub renders newlines literally); test plan = `make check`.
