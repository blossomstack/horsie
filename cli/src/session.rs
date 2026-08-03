//! Commands against `horsie-server`: tail a session's events to a local JSONL
//! file (with idempotent resume via the journal sequence cursor), list
//! sessions, and show one session's status.

use crate::error::CliError;
use crate::server_client::ServerClient;
use futures_util::StreamExt;
use horsie_models::now_ms;
use horsie_models::session::{SessionDetail, SessionEvent, SessionSummary};
use reqwest_eventsource::{Event, EventSource};
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// One JSONL line: the journal sequence (absent for ephemeral events) plus
/// the verbatim `SessionEvent` (serialized as `{"type":…,"value":…}`).
#[derive(Serialize)]
struct Envelope<'a> {
    seq: Option<u64>,
    event: &'a SessionEvent,
}

/// Live-only frames the server sends without an SSE id: deltas, tool starts,
/// status changes, errors, and progression. Everything else (Message,
/// ToolResult, TurnCompleted, TaskListChanged, Asked) is journaled and
/// id-stamped.
fn is_ephemeral(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Delta(_)
            | SessionEvent::ToolStart(_)
            | SessionEvent::StatusChanged(_)
            | SessionEvent::Error(_)
            | SessionEvent::Progressed(_)
    )
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
        let id: Option<u64> = if sse_id.is_empty() {
            None
        } else {
            match sse_id.parse() {
                Ok(s) => Some(s),
                Err(_) => {
                    eprintln!("warning: non-numeric SSE id '{sse_id}'; ignoring it");
                    None
                }
            }
        };
        let event: SessionEvent = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(e) => {
                // Can't tell durable from ephemeral; trust the id and skip
                // ahead so a reconnect doesn't replay the corrupt event.
                if let Some(s) = id {
                    self.cursor = Some(s);
                }
                eprintln!("warning: skipping unparseable event: {e}");
                return Ok(false);
            }
        };
        // Ephemeral frames are sent live-only without an `id:` field, but per
        // the SSE spec the client reports the stream's *last* id for them.
        // Null the stamp and leave the cursor alone — only durable (journaled)
        // events carry a meaningful sequence.
        let seq = if is_ephemeral(&event) { None } else { id };
        if let Some(s) = seq {
            self.cursor = Some(s);
        }
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

/// Reconnect backoff cap: starts at 1s, doubles per failed connection.
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
    eprintln!(
        "tailing session {session_id} → {} (Ctrl-C to stop)",
        path.display()
    );

    let client = reqwest::Client::new();
    let url = format!(
        "{}/api/sessions/{session_id}/events",
        server.trim_end_matches('/')
    );
    let mut backoff = Duration::from_secs(1);
    // One pinned Ctrl-C future for the whole tail: a fresh `ctrl_c()` only
    // fires on signals received *after* its creation, so re-creating it per
    // select iteration (or having none alive during backoff) loses signals.
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    // Outer loop: one iteration per (re)connection. Inner loop: consume the
    // stream; `break` reconnects, `return` exits (Ctrl-C, unknown session).
    loop {
        let mut req = client.get(&url);
        if let Some(seq) = sink.cursor() {
            req = req.header("Last-Event-ID", seq.to_string());
        }
        let mut es =
            EventSource::new(req).map_err(|e| CliError::Server(format!("connect {url}: {e}")))?;
        loop {
            tokio::select! {
                _ = &mut ctrl_c => {
                    es.close();
                    sink.flush()?;
                    return Ok(());
                }
                ev = es.next() => match ev {
                    None => break,
                    Some(Ok(Event::Open)) => backoff = Duration::from_secs(1),
                    Some(Ok(Event::Message(m))) => {
                        sink.handle(&m.id, &m.data)?;
                    }
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
        // Ctrl-C must interrupt the backoff too, not just the stream.
        tokio::select! {
            _ = &mut ctrl_c => {
                sink.flush()?;
                return Ok(());
            }
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
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

/// `horsie session list` — every session the server knows about.
pub async fn list(server: &str) -> Result<(), CliError> {
    let sessions = ServerClient::new(server).list_sessions().await?;
    print!("{}", render_session_table(&sessions, now_ms()));
    Ok(())
}

/// `horsie session status <id>` — a point-in-time snapshot (live progress is
/// `session tail`'s job).
pub async fn status(server: &str, session_id: &str) -> Result<(), CliError> {
    let detail = ServerClient::new(server).get_session(session_id).await?;
    print!("{}", render_session_detail(&detail, now_ms()));
    Ok(())
}

/// "just now", "5m ago", "3h ago", "2d ago".
fn relative(now_ms: u64, then_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn status_label(s: &Option<horsie_models::session::SessionStatusKind>) -> String {
    s.as_ref()
        .map(|k| format!("{k:?}"))
        .unwrap_or_else(|| "-".to_string())
}

fn render_session_table(sessions: &[SessionSummary], now: u64) -> String {
    if sessions.is_empty() {
        return "no sessions\n".to_string();
    }
    let mut out = format!(
        "{:<38} {:<24} {:<16} {:<10} LAST ERROR\n",
        "ID", "NAME", "STATUS", "CREATED"
    );
    for s in sessions {
        out.push_str(&format!(
            "{:<38} {:<24} {:<16} {:<10} {}\n",
            s.id,
            crate::agent::truncate(s.name.as_deref().unwrap_or("-"), 24),
            status_label(&s.status),
            relative(now, s.created_at),
            s.last_error.as_deref().unwrap_or(""),
        ));
    }
    out
}

fn render_session_detail(d: &SessionDetail, now: u64) -> String {
    let mut out = format!(
        "session     {}\nname        {}\nstatus      {}\ncreated     {}\nmodel       {}\nvendor      {}\n",
        d.id,
        d.name.as_deref().unwrap_or("-"),
        status_label(&d.status),
        relative(now, d.created_at),
        d.model,
        d.vendor,
    );
    if let Some(e) = d.thinking_effort.as_deref() {
        out.push_str(&format!("thinking    {e}\n"));
    }
    for r in &d.repos {
        out.push_str(&format!("repo        {r}\n"));
    }
    if !d.plugins.is_empty() {
        out.push_str(&format!("skills      {}\n", d.plugins.join(", ")));
    }
    if !d.mcp_servers.is_empty() {
        out.push_str(&format!("mcp         {}\n", d.mcp_servers.join(", ")));
    }
    if !d.memory_spaces.is_empty() {
        out.push_str(&format!("memory      {}\n", d.memory_spaces.join(", ")));
    }
    if let Some(err) = d.last_error.as_deref() {
        out.push_str(&format!("error       {err}\n"));
    }
    // Every unanswered ask, not just the first: a turn resumes only once all
    // of them are answered. `pending_question` is the pre-multi-ask fallback.
    if d.pending_asks.is_empty() {
        if let Some(q) = d.pending_question.as_deref() {
            out.push_str(&format!("awaiting    {q}\n"));
        }
    } else {
        for a in &d.pending_asks {
            out.push_str(&format!(
                "awaiting    {}\n",
                crate::agent::truncate(&a.question, 70)
            ));
        }
    }
    if !d.inbox.is_empty() {
        out.push_str(&format!("inbox       {} queued\n", d.inbox.len()));
        for m in &d.inbox {
            out.push_str(&format!("  · {}\n", crate::agent::truncate(&m.text, 70)));
        }
    }
    out
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
        assert_eq!(scan_last_seq(&dir.path().join("nope.jsonl")).unwrap(), None);
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

    fn sink(
        mode: EventsMode,
        cursor: Option<u64>,
    ) -> (SessionSink, tempfile::TempDir, std::path::PathBuf) {
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
            message: horsie_models::agent::Message::user("m1", "hi", 0),
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
                at_ms: 0,
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
    fn ephemeral_event_with_inherited_sse_id_is_nulled_and_does_not_move_cursor() {
        // Per the SSE spec, an event without `id:` inherits the stream's last
        // id; the server sends Deltas id-less, so reqwest-eventsource reports
        // the previous durable seq (here "12", ahead of the cursor at 9).
        // The envelope must still record null and the cursor must not move.
        let (mut s, _dir, path) = sink(EventsMode::All, Some(9));
        let data = serde_json::to_string(&SessionEvent::Delta(DeltaEvent {
            text: "chunk".into(),
        }))
        .unwrap();
        assert!(s.handle("12", &data).unwrap());
        s.flush().unwrap();
        let got = lines(&path);
        assert_eq!(got[0]["seq"], serde_json::Value::Null);
        assert_eq!(s.cursor(), Some(9));
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

    #[test]
    fn messages_mode_filters_to_complete_messages_only() {
        let msg = SessionEvent::Message(MessageEvent {
            message: horsie_models::agent::Message::user("m1", "hi", 0),
        });
        let delta = SessionEvent::Delta(DeltaEvent { text: "h".into() });
        assert!(EventsMode::Messages.allows(&msg));
        assert!(!EventsMode::Messages.allows(&delta));
        assert!(EventsMode::All.allows(&delta));
    }

    fn summary(id: &str, name: Option<&str>) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            name: name.map(Into::into),
            status: Some(horsie_models::session::SessionStatusKind::Running),
            created_at: 1_000,
            last_error: None,
        }
    }

    #[test]
    fn session_table_lists_status_and_relative_time() {
        let out = render_session_table(&[summary("s-1", Some("review"))], 1_000 + 5 * 60_000);
        assert!(out.contains("s-1"));
        assert!(out.contains("review"));
        assert!(out.contains("Running"));
        assert!(out.contains("5m ago"));
    }

    #[test]
    fn empty_session_table_says_no_sessions() {
        assert_eq!(render_session_table(&[], 0), "no sessions\n");
    }

    #[test]
    fn relative_buckets() {
        assert_eq!(relative(10_000, 0), "just now");
        assert_eq!(relative(5 * 60_000, 0), "5m ago");
        assert_eq!(relative(3 * 3_600_000, 0), "3h ago");
        assert_eq!(relative(2 * 86_400_000, 0), "2d ago");
    }

    #[test]
    fn detail_shows_awaiting_question_and_inbox() {
        let d = SessionDetail {
            id: "s-1".into(),
            name: None,
            status: Some(horsie_models::session::SessionStatusKind::AwaitingInput),
            created_at: 0,
            last_error: None,
            pending_question: Some("which file?".into()),
            pending_asks: vec![
                horsie_models::session::PendingAskView {
                    tool_call_id: Some("t1".into()),
                    question: "which file?".into(),
                },
                horsie_models::session::PendingAskView {
                    tool_call_id: Some("t2".into()),
                    question: "which branch?".into(),
                },
            ],
            model: "sonnet".into(),
            vendor: "local".into(),
            repos: vec![],
            plugins: vec![],
            mcp_servers: vec![],
            memory_spaces: vec![],
            use_plugins: false,
            thinking_effort: None,
            inbox: vec![horsie_models::session::QueuedMessage {
                id: "m1".into(),
                text: "follow up".into(),
                at_ms: 0,
            }],
        };
        let out = render_session_detail(&d, 0);
        assert!(out.contains("awaiting    which file?"));
        assert!(
            out.contains("awaiting    which branch?"),
            "every unanswered ask is listed: {out}"
        );
        assert!(out.contains("inbox       1 queued"));
        assert!(out.contains("· follow up"));
    }
}
