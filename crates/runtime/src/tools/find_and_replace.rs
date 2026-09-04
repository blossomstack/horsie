use horsie_models::runtime::{FindAndReplaceInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: FindAndReplaceInput) -> ToolResult {
    let path = working_dir.join(&input.path);
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
            return Err(no_match_error(
                &input.path,
                &content,
                &input.find,
                use_regex,
            ));
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
        Ok::<String, String>(match super::snippet::numbered_window(&new_content, s, e) {
            // One contiguous edit (or several close together): show it.
            Some(window) => format!(
                "Replaced {count} occurrence(s) in '{}'.\n\n{window}",
                input.path
            ),
            // Sites too far apart to window usefully. Line numbers are what
            // the model needs to re-anchor, at a fraction of the bytes.
            None => match hit_lines(&content, &input, &re) {
                Some(lines) => format!(
                    "Replaced {count} occurrence(s) in '{}' at lines {}.",
                    input.path,
                    super::snippet::line_list(&lines)
                ),
                None => format!("Replaced {count} occurrence(s) in '{}'.", input.path),
            },
        })
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: Vec::new(),
            original_output_bytes: 0,
            spilled_output_bytes: 0,
        }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
    }
}

/// 1-based line numbers, in the *new* content, where each replacement landed.
///
/// Literal mode only: a regex replacement expands `$1`-style references, so its
/// length — and the line shift it causes downstream — isn't knowable without
/// re-running the substitution site by site. `None` means "can't say", and the
/// caller falls back to a bare count.
fn hit_lines(
    content: &str,
    input: &FindAndReplaceInput,
    re: &Option<regex::Regex>,
) -> Option<Vec<usize>> {
    if re.is_some() || input.find.is_empty() {
        return None;
    }
    let find_newlines = input.find.matches('\n').count() as isize;
    let replace_newlines = input.replace.matches('\n').count() as isize;
    let mut lines = Vec::new();
    // `line`/`cursor` track a position in the old content; `shift` accumulates
    // how far earlier replacements have moved everything after them.
    let mut line: isize = 1;
    let mut cursor = 0usize;
    let mut shift: isize = 0;
    for (offset, _) in content.match_indices(input.find.as_str()) {
        line += content[cursor..offset].matches('\n').count() as isize;
        cursor = offset;
        lines.push((line + shift).max(1) as usize);
        shift += replace_newlines - find_newlines;
    }
    Some(lines)
}

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
        [(a, b)] => {
            let shown = match super::snippet::numbered_window(content, *a, *b) {
                Some(w) => format!(":\n{w}\n"),
                None => ". ".to_string(),
            };
            format!(
                "find target not found as-is, but lines {a}-{b} match ignoring \
                 indentation/whitespace{shown}Adjust `find` to match the file exactly, \
                 or use replace_lines with that range."
            )
        }
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
    if needle.is_empty() || needle.iter().all(String::is_empty) {
        return Vec::new();
    }
    let hay: Vec<String> = content.lines().map(normalize_ws).collect();
    if hay.len() < needle.len() {
        return Vec::new();
    }
    let whole_line: Vec<(usize, usize)> = (0..=hay.len() - needle.len())
        .filter(|&i| hay[i..i + needle.len()] == needle[..])
        .map(|i| (i + 1, i + needle.len()))
        .collect();
    if !whole_line.is_empty() || needle.len() > 1 {
        return whole_line;
    }
    // `find` is a single line and matched no line outright. It is probably a
    // mid-line fragment (`foo(bar)` inside `let x = foo(bar);`), which the
    // whole-line comparison can never hit — retry as a normalized substring.
    hay.iter()
        .enumerate()
        .filter(|(_, line)| line.contains(needle[0].as_str()))
        .map(|(i, _)| (i + 1, i + 1))
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn input(path: &str, find: &str, replace: &str) -> FindAndReplaceInput {
        FindAndReplaceInput {
            path: path.to_string(),
            find: find.to_string(),
            replace: replace.to_string(),
            regex: None,
            replace_all: None,
        }
    }

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
                assert!(
                    e.reason.contains("lines 1-2 match ignoring"),
                    "{}",
                    e.reason
                );
                assert!(e.reason.contains("2→     let x = 1;"), "{}", e.reason);
            }
        }
    }

    #[tokio::test]
    async fn no_match_reports_multiple_near_misses() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "foo bar\nfoo   bar\nxyz\n").unwrap();
        // Double-space `find` matches nothing literally, but both foo/bar lines
        // match once internal whitespace is collapsed.
        let result = exec(dir.path(), input("f.txt", "foo  bar", "z")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                assert!(
                    e.reason.contains("2 regions match ignoring"),
                    "{}",
                    e.reason
                );
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
                assert!(
                    e.reason.contains("also checked ignoring whitespace"),
                    "{}",
                    e.reason
                );
            }
        }
    }

    #[tokio::test]
    async fn unique_literal_match_is_replaced() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();
        let result = exec(dir.path(), input("f.txt", "world", "rust")).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("1 occurrence"));
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "hello rust");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn ambiguous_literal_match_is_an_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\na\n").unwrap();
        let result = exec(dir.path(), input("f.txt", "a", "b")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("matched 2 times"), "{}", e.reason);
                // File must be left untouched on a rejected edit.
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "a\na\n");
            }
        }
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a a a").unwrap();
        let mut i = input("f.txt", "a", "b");
        i.replace_all = Some(true);
        let result = exec(dir.path(), i).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("3 occurrence"), "{}", o.stdout);
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "b b b");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// A rename hitting both ends of a file spans nearly every line, so the
    /// window is declined and the confirmation names the sites instead. The old
    /// behaviour pasted ~1.5 KB of untouched context and still cut off the
    /// second edit.
    #[tokio::test]
    async fn scattered_replace_all_reports_line_numbers_not_a_window() {
        let dir = TempDir::new().unwrap();
        let body: String = (1..=200)
            .map(|i| {
                if i == 2 || i == 199 {
                    "TARGET\n".to_string()
                } else {
                    format!("line{i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("f.txt"), body).unwrap();
        let mut i = input("f.txt", "TARGET", "FIXED");
        i.replace_all = Some(true);
        match exec(dir.path(), i).await {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("2 occurrence"), "{}", o.stdout);
                assert!(o.stdout.contains("at lines 2, 199"), "{}", o.stdout);
                assert!(!o.stdout.contains("line50"), "pasted context: {}", o.stdout);
                assert!(o.stdout.len() < 100, "too big: {}", o.stdout.len());
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// Line numbers are reported against the *new* content, so a replacement
    /// that adds lines must shift every later site.
    #[tokio::test]
    async fn replace_all_line_numbers_account_for_added_lines() {
        let dir = TempDir::new().unwrap();
        let body: String = (1..=100)
            .map(|i| {
                if i == 10 || i == 90 {
                    "T\n".to_string()
                } else {
                    format!("line{i}\n")
                }
            })
            .collect();
        std::fs::write(dir.path().join("f.txt"), body).unwrap();
        let mut i = input("f.txt", "T", "A\nB");
        i.replace_all = Some(true);
        match exec(dir.path(), i).await {
            // Site 1 stays at line 10; site 2 shifts down by the line the first
            // replacement added.
            ToolResult::Ok(o) => assert!(o.stdout.contains("at lines 10, 91"), "{}", o.stdout),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// A `find` that is a fragment of a line can never equal a whole line, so
    /// the whole-line near-miss pass always misses it.
    #[tokio::test]
    async fn near_miss_finds_a_mid_line_fragment() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("f.rs"),
            "fn main() {\n    let x = foo(bar);\n}\n",
        )
        .unwrap();
        // Doubled internal space: no literal match, and not a whole line either
        // (the file's line has a trailing `;`) — only the substring pass finds it.
        let result = exec(
            dir.path(),
            input("f.rs", "let  x = foo(bar)", "let y = baz()"),
        )
        .await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("lines 2-2"), "{}", e.reason);
                assert!(
                    e.reason.contains("2→     let x = foo(bar);"),
                    "{}",
                    e.reason
                );
            }
        }
    }

    #[tokio::test]
    async fn regex_mode_uses_capture_groups() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "key=value").unwrap();
        let mut i = input("f.txt", r"(\w+)=(\w+)", "$2=$1");
        i.regex = Some(true);
        let result = exec(dir.path(), i).await;
        match result {
            ToolResult::Ok(_) => {
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "value=key");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn missing_target_is_an_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
        let result = exec(dir.path(), input("f.txt", "missing", "x")).await;
        match result {
            ToolResult::Ok(o) => panic!("expected error, got {}", o.stdout),
            ToolResult::Err(e) => assert!(e.reason.contains("not found"), "{}", e.reason),
        }
    }
}
