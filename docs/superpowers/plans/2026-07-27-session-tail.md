# `horsie session tail` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `horsie session tail <session-id> --output <path>` to the Rust CLI, streaming session events from `horsie-server`'s SSE endpoint to a local JSONL file with idempotent resume.

**Architecture:** Pure SSE tail against `GET /api/sessions/:id/events` using `reqwest-eventsource` (same crate `providers/openai` uses). Backfill, live tail, and reconnect are one mechanism: resume scans the output file for the last journal sequence number and sends it as `Last-Event-ID`. Each line is a `{"seq": Option<u64>, "event": SessionEvent}` envelope.

**Tech Stack:** Rust, clap 4 (derive), reqwest 0.12, reqwest-eventsource 0.6, futures-util, tokio, serde_json. Spec: `docs/superpowers/specs/2026-07-27-session-tail-design.md`.

## Global Constraints

- Work happens in the worktree `.horsie/worktrees/session-tail` on branch `feat/session-tail`. All paths below are relative to it.
- Production code must not use `unwrap`/`expect`/`panic` or wildcard enum match arms (workspace lints deny them). Test modules opt out via the crate's existing `#![cfg_attr(test, allow(...))]` in `main.rs`; for `session.rs` tests, put `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm))]` at the top of the `mod tests` block? — No: inner attributes on modules. Match the repo pattern instead: add the same `allow` attributes as `#[allow(...)]` on the `mod tests` item if lints fire in tests.
- `SessionEvent` serializes with serde `tag = "type", content = "value"` (fluorite-generated). The JSONL event field therefore looks like `{"type":"Message","value":{...}}`.
- `SessionEvent` and payload types live at `horsie_models::session::*`; generated structs have public fields (struct literals work) and also derive `derive_new::new`.
- No authentication anywhere in this feature.
- Import style: `imports_granularity = "Crate"`, vertical layout (rustfmt enforces; run `cargo fmt` before committing).
- Default server URL: `http://127.0.0.1:3789` (matches `horsie-server`'s default bind).
- Verify with: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`.

---

### Task 1: Module scaffold — `EventsMode`, output-path resolution, append writer

**Files:**
- Create: `cli/src/session.rs`
- Modify: `cli/src/lib.rs` (add `pub mod session;`)
- Modify: `cli/Cargo.toml` (add deps)

**Interfaces:**
- Produces (used by Tasks 2–4):
  - `pub enum EventsMode { Messages, All }` — derives `Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum`
  - `impl EventsMode { fn allows(self, event: &SessionEvent) -> bool }`
  - `fn output_path(output: &Path, session_id: &str) -> PathBuf`
  - `fn open_append(path: &Path) -> Result<BufWriter<File>, CliError>`

- [ ] **Step 1: Add dependencies to `cli/Cargo.toml`**

In the `[dependencies]` section, after the `eval` line:

```toml
reqwest           = { workspace = true, features = ["stream"] }
reqwest-eventsource = { workspace = true }
futures-util      = { workspace = true }
```

(`futures-util` is needed for `StreamExt::next()` on the `EventSource`.)

- [ ] **Step 2: Write the failing tests**

Create `cli/src/session.rs` with only this test module (implementation comes in Step 4):

```rust
//! Stream a session's events from `horsie-server`'s SSE endpoint to a local
//! JSONL file, with idempotent resume via the journal sequence cursor.

use horsie_models::session::SessionEvent;

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
```

`horsie_models::agent::Message::user(id, text)` is a hand-written helper in `models/src/lib.rs` — use it for message fixtures.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p horsie --lib session::`
Expected: FAIL — `output_path`, `open_append`, `EventsMode` unresolved.

- [ ] **Step 4: Implement the scaffold**

Insert above the test module in `cli/src/session.rs`:

```rust
use crate::error::CliError;
use std::fs::{File, OpenOptions};
use std::io::BufWriter;
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
```

Add `pub mod session;` to `cli/src/lib.rs` (alphabetical: after `plugins`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p horsie --lib session::`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add cli/Cargo.toml cli/src/lib.rs cli/src/session.rs Cargo.lock
git commit -m "feat(cli): session tail scaffold — events mode, output path, append writer"
```

---

### Task 2: Resume cursor — scan an existing file for the last journal sequence

**Files:**
- Modify: `cli/src/session.rs`

**Interfaces:**
- Consumes: nothing from Task 1 beyond the module itself.
- Produces (used by Task 4): `fn scan_last_seq(path: &Path) -> Result<Option<u64>, CliError>` — `Ok(None)` when the file is absent/empty/has no sequenced line.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `cli/src/session.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie --lib session::`
Expected: FAIL — `scan_last_seq` unresolved.

- [ ] **Step 3: Implement `scan_last_seq`**

Add to `cli/src/session.rs`:

```rust
use std::io::BufRead;
use std::io::BufReader;

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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie --lib session::`
Expected: PASS (9 tests).

- [ ] **Step 5: Commit**

```bash
git add cli/src/session.rs
git commit -m "feat(cli): resume cursor scan for session tail"
```

---

### Task 3: `SessionSink` — envelope, filter, cursor, one line per event

**Files:**
- Modify: `cli/src/session.rs`

**Interfaces:**
- Consumes: `EventsMode` (Task 1), `open_append` (Task 1, only for the constructor signature Task 4 uses).
- Produces (used by Task 4; all module-private):
  - `struct SessionSink { out: BufWriter<File>, cursor: Option<u64>, mode: EventsMode }`
  - `impl SessionSink { fn new(out: BufWriter<File>, cursor: Option<u64>, mode: EventsMode) -> Self; fn cursor(&self) -> Option<u64>; fn flush(&mut self) -> Result<(), CliError>; fn handle(&mut self, sse_id: &str, data: &str) -> Result<bool, CliError> }` — `handle` returns whether a line was written.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests`:

```rust
    fn sink(mode: EventsMode, cursor: Option<u64>) -> (SessionSink, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let out = open_append(&path).unwrap();
        (SessionSink::new(out, cursor, mode), dir, path)
    }

    fn lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn durable_message_is_written_with_its_seq_and_tagged_union_shape() {
        let (mut s, _dir, path) = sink(EventsMode::All, None);
        // A MessageEvent for a user message; the SSE id is the journal seq.
        let data = serde_json::to_string(&SessionEvent::Message(MessageEvent {
            message: horsie_models::agent::Message::user("m1", "hi"),
        }))
        .unwrap();
        assert!(s.handle("42", &data).unwrap());
        s.flush().unwrap();
        let got = lines(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["seq"], serde_json::json!(42));
        assert_eq!(got[0]["event"]["type"], serde_json::json!("Message"));
        assert!(got[0]["event"]["value"]["message"].is_object());
        assert_eq!(s.cursor(), Some(42));
    }

    #[test]
    fn ephemeral_event_gets_null_seq_but_still_written_in_all_mode() {
        let (mut s, _dir, path) = sink(EventsMode::All, Some(9));
        let data = serde_json::to_string(&SessionEvent::Delta(DeltaEvent {
            text: "chunk".into(),
        }))
        .unwrap();
        assert!(s.handle("", &data).unwrap());
        s.flush().unwrap();
        let got = lines(&path);
        assert_eq!(got[0]["seq"], serde_json::Value::Null);
        // An id-less event must NOT move the resume cursor.
        assert_eq!(s.cursor(), Some(9));
    }

    #[test]
    fn messages_mode_skips_non_messages_but_advances_the_cursor() {
        let (mut s, _dir, path) = sink(EventsMode::Messages, None);
        let tool = serde_json::to_string(&SessionEvent::ToolResult(
            horsie_models::session::ToolOutputEvent {
                tool_call_id: "t1".into(),
                output: "ok".into(),
                is_error: false,
            },
        ))
        .unwrap();
        assert!(!s.handle("10", &tool).unwrap());
        s.flush().unwrap();
        assert!(lines(&path).is_empty());
        // Cursor advanced past the skipped durable event, so a reconnect does
        // not replay it.
        assert_eq!(s.cursor(), Some(10));
    }

    #[test]
    fn unparseable_data_is_skipped_with_a_warning() {
        let (mut s, _dir, path) = sink(EventsMode::All, None);
        assert!(!s.handle("3", "{not json").unwrap());
        s.flush().unwrap();
        assert!(lines(&path).is_empty());
        // The durable seq still counts — the event is gone, don't replay it.
        assert_eq!(s.cursor(), Some(3));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie --lib session::`
Expected: FAIL — `SessionSink` unresolved.

- [ ] **Step 3: Implement `SessionSink`**

Add to `cli/src/session.rs`:

```rust
use serde::Serialize;
use std::io::Write;

/// One JSONL line: the journal sequence (absent for ephemeral events) plus
/// the verbatim `SessionEvent` (serialized as `{"type":…,"value":…}`).
#[derive(Serialize)]
struct Envelope<'a> {
    seq: Option<u64>,
    event: &'a SessionEvent,
}

/// Appends filtered session events to the output file, tracking the resume
/// cursor (last durable journal sequence seen, written or not).
struct SessionSink {
    out: BufWriter<File>,
    cursor: Option<u64>,
    mode: EventsMode,
}

impl SessionSink {
    fn new(out: BufWriter<File>, cursor: Option<u64>, mode: EventsMode) -> Self {
        Self { out, cursor, mode }
    }

    fn cursor(&self) -> Option<u64> {
        self.cursor
    }

    fn flush(&mut self) -> Result<(), CliError> {
        self.out
            .flush()
            .map_err(|e| CliError::Io(format!("flush output: {e}")))
    }

    /// Process one SSE message frame. Returns whether a line was written.
    /// Parse failures warn and are skipped (mirrors the server's own
    /// log-and-skip posture); the cursor still advances past durable ids so a
    /// reconnect never replays a skipped event.
    fn handle(&mut self, sse_id: &str, data: &str) -> Result<bool, CliError> {
        let seq: Option<u64> = if sse_id.is_empty() {
            None
        } else {
            match sse_id.parse() {
                Ok(s) => Some(s),
                Err(_) => {
                    eprintln!("warning: non-numeric SSE id '{sse_id}'; treating as ephemeral");
                    None
                }
            }
        };
        if let Some(s) = seq {
            self.cursor = Some(s);
        }
        let event: SessionEvent = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warning: skipping unparseable event: {e}");
                return Ok(false);
            }
        };
        if !self.mode.allows(&event) {
            return Ok(false);
        }
        let line = serde_json::to_string(&Envelope { seq, event: &event })
            .map_err(|e| CliError::Io(format!("serialize event: {e}")))?;
        writeln!(self.out, "{line}").map_err(|e| CliError::Io(format!("write output: {e}")))?;
        // Flush per line: this is a long-running tail; a crash must not lose
        // events the user already considers archived.
        self.flush()?;
        Ok(true)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie --lib session::`
Expected: PASS (13 tests).

- [ ] **Step 5: Commit**

```bash
git add cli/src/session.rs
git commit -m "feat(cli): session event sink — JSONL envelope, filtering, cursor tracking"
```

---

### Task 4: The SSE tail loop + CLI wiring

**Files:**
- Modify: `cli/src/session.rs` (add `pub async fn tail`)
- Modify: `cli/src/error.rs` (add `Server` variant)
- Modify: `cli/src/main.rs` (add `Session` subcommand + dispatch)

**Interfaces:**
- Consumes: `EventsMode`, `output_path`, `scan_last_seq`, `open_append`, `SessionSink` (Tasks 1–3).
- Produces: `pub async fn tail(server: &str, session_id: &str, output: &Path, mode: EventsMode) -> Result<(), CliError>` — called from `main.rs` dispatch; runs until Ctrl-C (exit 0).

- [ ] **Step 1: Add the `Server` error variant**

In `cli/src/error.rs`, add before the closing brace of the enum:

```rust
    #[error("session server error: {0}")]
    Server(String),
```

- [ ] **Step 2: Implement the tail loop**

Add to `cli/src/session.rs`:

```rust
use futures_util::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use std::time::Duration;

/// Reconnect backoff: 1s doubling, capped at 30s. Reset on each successful
/// (re)connect.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Stream a session's events into `output` until Ctrl-C. Backfill, live
/// tail, and reconnect are one mechanism: the server replays the journal
/// after the `Last-Event-ID` cursor, then bridges to the live broadcast.
pub async fn tail(
    server: &str,
    session_id: &str,
    output: &Path,
    mode: EventsMode,
) -> Result<(), CliError> {
    let path = output_path(output, session_id);
    let cursor = scan_last_seq(&path)?;
    let mut sink = SessionSink::new(open_append(&path)?, cursor, mode);
    eprintln!("tailing session {session_id} → {} (Ctrl-C to stop)", path.display());

    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/sessions/{session_id}/events",
        server.trim_end_matches('/')
    );
    let mut backoff = Duration::from_secs(1);
    // Outer loop: one iteration per (re)connection. Inner loop: consume the
    // stream; `break` reconnects, `return` exits (Ctrl-C, unknown session).
    loop {
        let mut req = client.get(&url);
        if let Some(seq) = sink.cursor() {
            req = req.header("Last-Event-ID", seq.to_string());
        }
        let mut es = EventSource::new(req)
            .map_err(|e| CliError::Server(format!("connect {url}: {e}")))?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    es.close();
                    sink.flush()?;
                    return Ok(());
                }
                ev = es.next() => match ev {
                    None => break,
                    Some(Ok(Event::Open)) => backoff = Duration::from_secs(1),
                    Some(Ok(Event::Message(m))) => sink.handle(&m.id, &m.data)?,
                    Some(Err(reqwest_eventsource::Error::InvalidStatusCode(status, _)))
                        if status == reqwest::StatusCode::NOT_FOUND =>
                    {
                        es.close();
                        return Err(CliError::Server(format!("no such session: {session_id}")));
                    }
                    Some(Err(reqwest_eventsource::Error::StreamEnded)) => break,
                    Some(Err(e)) => {
                        eprintln!("warning: stream error: {e}; reconnecting");
                        break;
                    }
                }
            }
        }
        es.close();
        eprintln!("disconnected; retrying in {}s", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}
```

- [ ] **Step 3: Wire the subcommand into `cli/src/main.rs`**

Add to the imports at the top:

```rust
use horsie::session::{self, EventsMode};
```

Add to `enum Command` (after the `Connect` variant):

```rust
    /// Commands against a session server (`horsie-server`).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
```

Add a new subcommand enum (next to `PluginAction`):

```rust
#[derive(Subcommand)]
enum SessionAction {
    /// Stream a session's messages to a local JSONL file until Ctrl-C.
    /// Resumes after the last recorded event when the output file exists.
    Tail {
        /// Session UUID on the server.
        session_id: String,
        /// Output file, or an existing directory to write
        /// `<session-id>.jsonl` into.
        #[arg(long)]
        output: PathBuf,
        /// Session server base URL.
        #[arg(long, default_value = "http://127.0.0.1:3789")]
        server: String,
        /// Which events to write.
        #[arg(long, value_enum, default_value = "messages")]
        events: EventsMode,
    },
}
```

Add the dispatch arm in `dispatch`, right after the `Command::Plugin { action } => match action { … }` arm (i.e. before the `Command::Connect` arm), matching existing style:

```rust
        Command::Session { action } => match action {
            SessionAction::Tail {
                session_id,
                output,
                server,
                events,
            } => {
                session::tail(&server, &session_id, &output, events).await?;
                Ok(0)
            }
        },
```

- [ ] **Step 4: Build and run the full unit suite**

Run: `cargo test -p horsie`
Expected: PASS (13 session tests + existing CLI tests), clean compile.

- [ ] **Step 5: Smoke-check the CLI surface**

Run: `cargo run -p horsie -- session tail --help`
Expected: help text shows `--output`, `--server` (default `http://127.0.0.1:3789`), `--events` (default `messages`).

- [ ] **Step 6: Commit**

```bash
git add cli/src/session.rs cli/src/error.rs cli/src/main.rs
git commit -m "feat(cli): horsie session tail — SSE stream to JSONL with resume"
```

---

### Task 5: Pre-PR verification

**Files:** none (verification only; fixes go in the offending files).

- [ ] **Step 1: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. Fix any in-place (common hits: `needless_borrow`, `let_and_return`, doc-lazy continuation in the new doc comments).

- [ ] **Step 2: Format**

Run: `cargo fmt --check`
Expected: clean. If not: `cargo fmt` and include the result in the final commit.

- [ ] **Step 3: Full workspace tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Manual E2E (document the result in the PR description)**

With a `horsie-server` running locally and at least one session with history:
1. `horsie session tail <id> --output /tmp/tail/` → file `/tmp/tail/<id>.jsonl` grows; first lines are the journaled history.
2. Send a message to the session; new lines appear live.
3. Ctrl-C; re-run the same command → no duplicate lines (compare `wc -l` before/after and diff the files).
4. `horsie session tail <bad-id> --output /tmp/x.jsonl` → exits 1 with `no such session`.
5. `--events all` → file also contains `Delta`/`ToolStart` lines with `"seq":null`.

- [ ] **Step 5: Final commit (only if fmt/clippy fixes were needed)**

```bash
git add -A
git commit -m "chore(cli): clippy/fmt fixes for session tail"
```
