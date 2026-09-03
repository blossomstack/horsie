use horsie_models::runtime::{ApplyPatchInput, ToolError, ToolOutput, ToolResult};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const UPDATE_FILE: &str = "*** Update File: ";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const MAX_PATCH_BYTES: usize = 1_000_000;
const MAX_FILES: usize = 100;
const MAX_HUNKS: usize = 500;

pub async fn exec(working_dir: &Path, input: ApplyPatchInput) -> ToolResult {
    let patch = input.patch;
    if patch.len() > MAX_PATCH_BYTES {
        return error(format!(
            "patch is {} bytes; the maximum is {MAX_PATCH_BYTES}",
            patch.len()
        ));
    }

    // Parsing is deliberately complete before the blocking task starts. A
    // malformed footer or late file section must never let an earlier section
    // reach the filesystem.
    let parsed = match parse_patch(&patch) {
        Ok(parsed) => parsed,
        Err(reason) => return error(reason),
    };
    let root = working_dir.to_path_buf();

    match tokio::task::spawn_blocking(move || apply(&root, parsed)).await {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: Vec::new(),
        }),
        Ok(Err(reason)) => error(reason),
        Err(join_error) => error(join_error.to_string()),
    }
}

fn error(reason: String) -> ToolResult {
    ToolResult::Err(ToolError { reason })
}

#[derive(Debug, PartialEq)]
enum FilePatch {
    Update { path: String, hunks: Vec<Hunk> },
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
}

impl FilePatch {
    fn path(&self) -> &str {
        match self {
            Self::Update { path, .. } | Self::Add { path, .. } | Self::Delete { path } => path,
        }
    }
}

#[derive(Debug, PartialEq)]
struct Hunk {
    lines: Vec<HunkLine>,
}

#[derive(Debug, PartialEq)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Parse the complete patch document without touching the filesystem.
fn parse_patch(input: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.first() != Some(&BEGIN_PATCH) {
        return Err(format!("line 1 must be exactly '{BEGIN_PATCH}'"));
    }
    if lines.last() != Some(&END_PATCH) {
        return Err(format!("the last line must be exactly '{END_PATCH}'"));
    }
    if lines.len() == 2 {
        return Err("patch contains no file sections".to_string());
    }

    let mut patches = Vec::new();
    let mut paths = HashSet::new();
    let mut hunk_count = 0usize;
    let mut index = 1usize;
    while index + 1 < lines.len() {
        let header = lines[index];
        if let Some(raw_path) = header.strip_prefix(UPDATE_FILE) {
            let path = parse_path(raw_path, index + 1, &mut paths)?;
            index += 1;
            let mut hunks = Vec::new();
            while index + 1 < lines.len() && !is_file_header(lines[index]) {
                if !is_hunk_header(lines[index]) {
                    return Err(format!(
                        "line {}: expected '@@' to start an update hunk, found {:?}",
                        index + 1,
                        lines[index]
                    ));
                }
                index += 1;
                let (hunk, next) = parse_hunk(&lines, index)?;
                hunks.push(hunk);
                hunk_count += 1;
                if hunk_count > MAX_HUNKS {
                    return Err(format!("patch has more than {MAX_HUNKS} hunks"));
                }
                index = next;
            }
            if hunks.is_empty() {
                return Err(format!("update for '{path}' contains no hunks"));
            }
            patches.push(FilePatch::Update { path, hunks });
        } else if let Some(raw_path) = header.strip_prefix(ADD_FILE) {
            let path = parse_path(raw_path, index + 1, &mut paths)?;
            index += 1;
            let mut added = Vec::new();
            while index + 1 < lines.len() && !is_file_header(lines[index]) {
                let Some(line) = lines[index].strip_prefix('+') else {
                    return Err(format!(
                        "line {}: every added-file line must start with '+'",
                        index + 1
                    ));
                };
                added.push(line.to_string());
                index += 1;
            }
            if added.is_empty() {
                return Err(format!("add for '{path}' contains no lines"));
            }
            patches.push(FilePatch::Add { path, lines: added });
        } else if let Some(raw_path) = header.strip_prefix(DELETE_FILE) {
            let path = parse_path(raw_path, index + 1, &mut paths)?;
            index += 1;
            if index + 1 < lines.len() && !is_file_header(lines[index]) {
                return Err(format!(
                    "line {}: a delete section cannot contain body lines",
                    index + 1
                ));
            }
            patches.push(FilePatch::Delete { path });
        } else {
            return Err(format!(
                "line {}: expected an Update File, Add File, or Delete File header, found {:?}",
                index + 1,
                header
            ));
        }

        if patches.len() > MAX_FILES {
            return Err(format!("patch changes more than {MAX_FILES} files"));
        }
    }

    Ok(patches)
}

fn parse_path(raw: &str, line: usize, seen: &mut HashSet<String>) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err(format!("line {line}: file path cannot be empty"));
    }
    if path.contains('\0') {
        return Err(format!("line {line}: file path contains a NUL byte"));
    }
    if !seen.insert(path.to_string()) {
        return Err(format!(
            "line {line}: '{path}' has more than one file section; put all of its hunks in one section"
        ));
    }
    Ok(path.to_string())
}

fn is_file_header(line: &str) -> bool {
    line.starts_with("*** ")
}

fn is_hunk_header(line: &str) -> bool {
    line == "@@" || line.starts_with("@@ ")
}

fn parse_hunk(lines: &[&str], start: usize) -> Result<(Hunk, usize), String> {
    let mut parsed = Vec::new();
    let mut has_change = false;
    let mut has_match = false;
    let mut index = start;
    while index + 1 < lines.len() && !is_hunk_header(lines[index]) && !is_file_header(lines[index])
    {
        let line = lines[index];
        let parsed_line = if let Some(value) = line.strip_prefix(' ') {
            has_match = true;
            HunkLine::Context(value.to_string())
        } else if let Some(value) = line.strip_prefix('-') {
            has_change = true;
            has_match = true;
            HunkLine::Remove(value.to_string())
        } else if let Some(value) = line.strip_prefix('+') {
            has_change = true;
            HunkLine::Add(value.to_string())
        } else {
            return Err(format!(
                "line {}: hunk lines must start with ' ', '+', or '-'",
                index + 1
            ));
        };
        parsed.push(parsed_line);
        index += 1;
    }
    if parsed.is_empty() {
        return Err(format!("line {}: update hunk is empty", start + 1));
    }
    if !has_change {
        return Err(format!("line {}: update hunk changes nothing", start + 1));
    }
    if !has_match {
        return Err(format!(
            "line {}: insertion-only hunks are ambiguous; include an unchanged context line",
            start + 1
        ));
    }
    Ok((Hunk { lines: parsed }, index))
}

#[derive(Debug)]
struct TextFile {
    lines: Vec<String>,
    separator: &'static str,
    trailing_newline: bool,
}

impl TextFile {
    fn read(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
        let separator = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        Ok(Self {
            lines: content.lines().map(str::to_string).collect(),
            separator,
            trailing_newline: content.ends_with('\n'),
        })
    }

    fn render(&self) -> String {
        let mut content = self.lines.join(self.separator);
        if self.trailing_newline && !self.lines.is_empty() {
            content.push_str(self.separator);
        }
        content
    }

    fn apply_hunk(
        &mut self,
        hunk: &Hunk,
        path: &str,
        number: usize,
    ) -> Result<(usize, usize), String> {
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(value) | HunkLine::Remove(value) => Some(value.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let new: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(value) | HunkLine::Add(value) => Some(value.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect();

        let matches: Vec<usize> = self
            .lines
            .windows(old.len())
            .enumerate()
            .filter(|(_, window)| window.iter().map(String::as_str).eq(old.iter().copied()))
            .map(|(index, _)| index)
            .collect();
        let start = match matches.as_slice() {
            [] => return Err(format!("{path}: hunk {number} did not match")),
            [start] => *start,
            many => {
                return Err(format!(
                    "{path}: hunk {number} matched {} locations; include more context",
                    many.len()
                ));
            }
        };
        let old_end = start + old.len();
        let new_len = new.len();
        self.lines.splice(start..old_end, new);
        Ok((start + 1, start + new_len.max(1)))
    }
}

#[derive(Debug)]
enum PlannedContent {
    Write(String),
    Delete,
}

#[derive(Debug)]
struct PlannedFile {
    path: PathBuf,
    shown_path: String,
    kind: char,
    content: PlannedContent,
    ranges: Vec<(usize, usize)>,
}

/// Validate every file precondition and hunk against in-memory content before
/// the first write. This is the behavioral boundary the tool promises: syntax
/// or context errors can never leave a half-applied patch.
fn apply(root: &Path, patch: Vec<FilePatch>) -> Result<String, String> {
    let mut planned = Vec::with_capacity(patch.len());
    for file_patch in patch {
        let shown_path = file_patch.path().to_string();
        let path = root.join(file_patch.path());
        match file_patch {
            FilePatch::Update { hunks, .. } => {
                if !path.is_file() {
                    return Err(format!("cannot update '{shown_path}': file does not exist"));
                }
                let mut file = TextFile::read(&path)?;
                let mut ranges = Vec::with_capacity(hunks.len());
                for (index, hunk) in hunks.iter().enumerate() {
                    ranges.push(file.apply_hunk(hunk, &shown_path, index + 1)?);
                }
                planned.push(PlannedFile {
                    path,
                    shown_path,
                    kind: 'M',
                    content: PlannedContent::Write(file.render()),
                    ranges,
                });
            }
            FilePatch::Add { lines, .. } => {
                if path.exists() {
                    return Err(format!("cannot add '{shown_path}': path already exists"));
                }
                let line_count = lines.len();
                let mut content = lines.join("\n");
                content.push('\n');
                planned.push(PlannedFile {
                    path,
                    shown_path,
                    kind: 'A',
                    content: PlannedContent::Write(content),
                    ranges: vec![(1, line_count)],
                });
            }
            FilePatch::Delete { .. } => {
                if !path.is_file() {
                    return Err(format!("cannot delete '{shown_path}': file does not exist"));
                }
                planned.push(PlannedFile {
                    path,
                    shown_path,
                    kind: 'D',
                    content: PlannedContent::Delete,
                    ranges: Vec::new(),
                });
            }
        }
    }

    for file in &planned {
        match &file.content {
            PlannedContent::Write(content) => {
                if file.kind == 'A'
                    && let Some(parent) = file.path.parent()
                {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!("failed to create directory '{}': {e}", parent.display())
                    })?;
                }
                std::fs::write(&file.path, content)
                    .map_err(|e| format!("failed to write '{}': {e}", file.path.display()))?;
            }
            PlannedContent::Delete => std::fs::remove_file(&file.path)
                .map_err(|e| format!("failed to delete '{}': {e}", file.path.display()))?,
        }
    }

    let mut output = format!("Applied patch to {} file(s).", planned.len());
    for file in planned {
        output.push('\n');
        output.push(file.kind);
        output.push(' ');
        output.push_str(&file.shown_path);
        if !file.ranges.is_empty() {
            let ranges = file
                .ranges
                .iter()
                .map(|(start, end)| {
                    if start == end {
                        start.to_string()
                    } else {
                        format!("{start}-{end}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(" (lines ");
            output.push_str(&ranges);
            output.push(')');
        }
    }
    Ok(output)
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

    fn input(patch: &str) -> ApplyPatchInput {
        ApplyPatchInput {
            patch: patch.to_string(),
        }
    }

    async fn output(dir: &TempDir, patch: &str) -> String {
        match exec(dir.path(), input(patch)).await {
            ToolResult::Ok(output) => output.stdout,
            ToolResult::Err(error) => panic!("{}", error.reason),
        }
    }

    async fn reason(dir: &TempDir, patch: &str) -> String {
        match exec(dir.path(), input(patch)).await {
            ToolResult::Err(error) => error.reason,
            ToolResult::Ok(output) => panic!("expected error, got {}", output.stdout),
        }
    }

    #[tokio::test]
    async fn applies_several_ordered_hunks_to_one_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
        let result = output(
            &dir,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n alpha\n-beta\n+BETA\n gamma\n@@\n gamma\n-delta\n+DELTA\n*** End Patch",
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
            "alpha\nBETA\ngamma\nDELTA\n"
        );
        assert!(result.contains("M one.txt (lines 1-3, 3-4)"), "{result}");
    }

    #[tokio::test]
    async fn applies_updates_adds_and_deletes_in_one_patch() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("old.txt"), "old\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "gone\n").unwrap();
        let result = output(
            &dir,
            "*** Begin Patch\n*** Update File: old.txt\n@@\n-old\n+new\n*** Add File: nested/new.txt\n+first\n+second\n*** Delete File: gone.txt\n*** End Patch",
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("old.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("nested/new.txt")).unwrap(),
            "first\nsecond\n"
        );
        assert!(!dir.path().join("gone.txt").exists());
        assert!(result.contains("M old.txt"), "{result}");
        assert!(result.contains("A nested/new.txt"), "{result}");
        assert!(result.contains("D gone.txt"), "{result}");
    }

    #[tokio::test]
    async fn validates_the_complete_syntax_before_applying_anything() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.txt"), "old\n").unwrap();
        let error = reason(
            &dir,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n-old\n+new\n*** Add File: broken.txt\nthis line has no plus\n*** End Patch",
        )
        .await;
        assert!(error.contains("must start with '+'"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
            "old\n"
        );
        assert!(!dir.path().join("broken.txt").exists());
    }

    #[tokio::test]
    async fn validates_every_hunk_before_applying_anything() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.txt"), "old\n").unwrap();
        std::fs::write(dir.path().join("two.txt"), "actual\n").unwrap();
        let error = reason(
            &dir,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n-old\n+new\n*** Update File: two.txt\n@@\n-missing\n+replacement\n*** End Patch",
        )
        .await;
        assert!(error.contains("two.txt: hunk 1 did not match"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
            "old\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("two.txt")).unwrap(),
            "actual\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_hunks_are_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.txt"), "same\nx\nsame\n").unwrap();
        let error = reason(
            &dir,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n-same\n+changed\n*** End Patch",
        )
        .await;
        assert!(error.contains("matched 2 locations"), "{error}");
    }

    #[tokio::test]
    async fn crlf_survives_an_update() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("one.txt"), "alpha\r\nbeta\r\n").unwrap();
        output(
            &dir,
            "*** Begin Patch\n*** Update File: one.txt\n@@\n alpha\n-beta\n+BETA\n*** End Patch",
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
            "alpha\r\nBETA\r\n"
        );
    }

    #[test]
    fn parser_accepts_a_standard_hunk_header_with_coordinates() {
        let patch =
            "*** Begin Patch\n*** Update File: a\n@@ -1,1 +1,1 @@\n-old\n+new\n*** End Patch";
        assert!(parse_patch(patch).is_ok());
    }

    #[test]
    fn parser_rejects_missing_delimiters_and_unknown_lines() {
        assert!(parse_patch("*** Update File: a\n@@\n-a\n+b").is_err());
        assert!(parse_patch("*** Begin Patch\n*** End Patch").is_err());
        assert!(parse_patch("*** Begin Patch\n*** Rewrite File: a\n*** End Patch").is_err());
        assert!(
            parse_patch("*** Begin Patch\n*** Update File: a\n@@\n-a\nb\n*** End Patch").is_err()
        );
    }

    #[test]
    fn parser_rejects_duplicate_files_and_insertion_without_context() {
        let duplicate = "*** Begin Patch\n*** Delete File: a\n*** Delete File: a\n*** End Patch";
        assert!(parse_patch(duplicate).is_err());
        let insertion = "*** Begin Patch\n*** Update File: a\n@@\n+new\n*** End Patch";
        assert!(parse_patch(insertion).is_err());
    }
}
