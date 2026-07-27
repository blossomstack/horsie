//! Stream a session's events from `horsie-server`'s SSE endpoint to a local
//! JSONL file, with idempotent resume via the journal sequence cursor.

use crate::error::CliError;
use horsie_models::session::SessionEvent;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// Which session events land in the JSONL file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EventsMode {
    /// Complete transcript messages only (the durable conversation).
    Messages,
    /// Every event: messages, deltas, tool starts/results, status changes, …
    All,
}

impl EventsMode {
    fn allows(self, event: &SessionEvent) -> bool {
        match self {
            EventsMode::Messages => matches!(event, SessionEvent::Message(_)),
            EventsMode::All => true,
        }
    }
}

/// `--output` semantics: an existing directory → `<session-id>.jsonl` inside
/// it; anything else is the file path itself.
fn output_path(output: &Path, session_id: &str) -> PathBuf {
    if output.is_dir() {
        output.join(format!("{session_id}.jsonl"))
    } else {
        output.to_path_buf()
    }
}

/// Open `path` for appending, creating it (and its parents) if needed.
fn open_append(path: &Path) -> Result<BufWriter<File>, CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Io(format!("create {}: {e}", parent.display())))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| CliError::Io(format!("open {}: {e}", path.display())))?;
    Ok(BufWriter::new(file))
}

/// Probe for resume: only `seq` matters. Deliberately NOT the full
/// `SessionEvent` — a line from an older/newer schema still yields its cursor.
#[derive(serde::Deserialize)]
struct SeqProbe {
    seq: Option<u64>,
}

/// Last journal sequence written to `path`, scanning forward with a buffered
/// reader (no whole-file load, no partial-line edge cases). Absent file →
/// `Ok(None)` (fresh tail from the beginning of the journal).
fn scan_last_seq(path: &Path) -> Result<Option<u64>, CliError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CliError::Io(format!("read {}: {e}", path.display()))),
    };
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| CliError::Io(format!("read {}: {e}", path.display())))?;
        if let Ok(probe) = serde_json::from_str::<SeqProbe>(&line)
            && probe.seq.is_some()
        {
            last = probe.seq;
        }
    }
    Ok(last)
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
    use horsie_models::session::{DeltaEvent, MessageEvent};

    #[test]
    fn output_path_inside_existing_dir_uses_session_id_filename() {
        let dir = tempfile::tempdir().unwrap();
        let p = output_path(dir.path(), "abc-123");
        assert_eq!(p, dir.path().join("abc-123.jsonl"));
    }

    #[test]
    fn output_path_for_plain_file_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.jsonl");
        assert_eq!(output_path(&target, "abc-123"), target);
    }

    #[test]
    fn output_path_for_missing_dir_is_treated_as_a_file() {
        // A path that does not exist (even one that looks like a dir) is a file
        // path; `open_append` creates the parents.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("deep");
        assert_eq!(output_path(&target, "abc-123"), target);
    }

    #[test]
    fn open_append_creates_parents_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("log.jsonl");
        {
            let mut w = open_append(&path).unwrap();
            use std::io::Write;
            writeln!(w, "one").unwrap();
            w.flush().unwrap();
        }
        {
            let mut w = open_append(&path).unwrap();
            use std::io::Write;
            writeln!(w, "two").unwrap();
            w.flush().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "one\ntwo\n");
    }

    #[test]
    fn scan_last_seq_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            scan_last_seq(&dir.path().join("nope.jsonl")).unwrap(),
            None
        );
    }

    #[test]
    fn scan_last_seq_empty_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert_eq!(scan_last_seq(&path).unwrap(), None);
    }

    #[test]
    fn scan_last_seq_returns_the_last_sequenced_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // A null-seq line after the last sequenced one must not win.
        std::fs::write(
            &path,
            "{\"seq\":1,\"event\":{}}\n{\"seq\":null,\"event\":{}}\n{\"seq\":41,\"event\":{}}\n{\"seq\":null,\"event\":{}}\n",
        )
        .unwrap();
        assert_eq!(scan_last_seq(&path).unwrap(), Some(41));
    }

    #[test]
    fn scan_last_seq_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "{\"seq\":7,\"event\":{}}\nnot-json\n").unwrap();
        assert_eq!(scan_last_seq(&path).unwrap(), Some(7));
    }

    #[test]
    fn messages_mode_filters_to_complete_messages_only() {
        let msg = SessionEvent::Message(MessageEvent {
            message: horsie_models::agent::Message::user("m1", "hi"),
        });
        let delta = SessionEvent::Delta(DeltaEvent { text: "h".into() });
        assert!(EventsMode::Messages.allows(&msg));
        assert!(!EventsMode::Messages.allows(&delta));
        assert!(EventsMode::All.allows(&delta));
    }
}
