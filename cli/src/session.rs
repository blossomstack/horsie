//! Commands against `horsie-server`: tail a session's events to a local JSONL
//! file (with idempotent resume via the journal sequence cursor), list
//! sessions, and show one session's status.

use crate::error::CliError;
use crate::server_client::ServerClient;
use futures_util::StreamExt;
use horsie_models::now_ms;
use horsie_models::session::{MessageFrame, SessionDetail, SessionSummary};
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
    /// Every frame: log entries and the live text chunks between them.
    All,
}

impl EventsMode {
    fn allows(self, frame: &MessageFrame) -> bool {
        match self {
            EventsMode::Messages => matches!(frame, MessageFrame::Entry(_)),
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

/// One JSONL line: the append cursor — the message id, absent for everything
/// that is not a transcript append — plus the verbatim `MessageFrame`
/// (serialized as `{"type":…,"value":…}`).
#[derive(Serialize)]
struct Envelope<'a> {
    id: Option<&'a str>,
    event: &'a MessageFrame,
}

/// The cursor to resume from after this frame.
///
/// A durable entry's `seq`; a delta's position within the entry it follows.
/// Both are resumable, which is new — the old stream could only resume from an
/// append, so a reconnect mid-message replayed the whole message.
fn resume_cursor(frame: &MessageFrame) -> String {
    match frame {
        MessageFrame::Entry(e) => e.seq.to_string(),
        MessageFrame::Delta(d) => format!("{}.{}", d.entry_seq, d.delta_seq),
    }
}

/// Appends filtered agent events to the output file, tracking the resume
/// cursor (the last appended message id seen, written or not).
struct SessionSink {
    out: BufWriter<File>,
    cursor: Option<String>,
    mode: EventsMode,
}

impl SessionSink {
    fn new(out: BufWriter<File>, cursor: Option<String>, mode: EventsMode) -> Self {
        Self { out, cursor, mode }
    }

    fn cursor(&self) -> Option<String> {
        self.cursor.clone()
    }

    fn flush(&mut self) -> Result<(), CliError> {
        self.out
            .flush()
            .map_err(|e| CliError::Io(format!("flush output: {e}")))
    }

    /// Process one SSE message frame. Returns whether a line was written.
    /// Parse failures warn and are skipped (mirrors the server's own
    /// log-and-skip posture); the cursor still advances past the frame's id so
    /// a reconnect never replays a skipped event.
    fn handle(&mut self, sse_id: &str, data: &str) -> Result<bool, CliError> {
        let event: MessageFrame = match serde_json::from_str(data) {
            Ok(e) => e,
            Err(e) => {
                // Can't tell an append from a value frame, so trust the stream's
                // id and skip ahead rather than re-receiving the corrupt frame.
                if !sse_id.is_empty() {
                    self.cursor = Some(sse_id.to_string());
                }
                eprintln!("warning: skipping unparseable event: {e}");
                return Ok(false);
            }
        };
        // Every frame carries a resumable position now — a delta included —
        // so a reconnect mid-message continues rather than replaying it. The
        // cursor is derived from the frame rather than taken from `sse_id`,
        // because the two must agree and only one of them is typed.
        let id = resume_cursor(&event);
        self.cursor = Some(id.clone());
        if !self.mode.allows(&event) {
            return Ok(false);
        }
        let line = serde_json::to_string(&Envelope {
            id: Some(id.as_str()),
            event: &event,
        })
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
    // `agent` picks the stream: `None` is the session-scoped one (status,
    // inbox, roster). A workflow run has no main agent, so reading one of its
    // steps means naming that step's agent.
    agent: Option<&str>,
) -> Result<(), CliError> {
    let path = output_path(output, session_id);
    let cursor = scan_last_message_id(&path)?;
    let mut sink = SessionSink::new(open_append(&path)?, cursor, mode);
    eprintln!(
        "tailing session {session_id} → {} (Ctrl-C to stop)",
        path.display()
    );

    let client = reqwest::Client::new();
    // Resolved once: a refresh mid-tail would be a new access token the
    // reconnect loop could not see anyway, and a tail that outlives its access
    // token reconnects and re-resolves from the top.
    let token = crate::auth::resolve_token(server).await?;
    // One endpoint, one stream. `aid` names the agent's log; absent means the
    // session's primary agent, and session-scoped events ride that log too.
    let url = format!(
        "{}/api/sessions/{session_id}/messages?aid={}",
        server.trim_end_matches('/'),
        agent.unwrap_or("main")
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
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
        if let Some(cursor) = sink.cursor() {
            req = req.header("Last-Event-ID", cursor);
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
                    // Reconnecting cannot fix a missing credential, so say what
                    // to do instead of retrying forever.
                    Some(Err(reqwest_eventsource::Error::InvalidStatusCode(status, _)))
                        if status == reqwest::StatusCode::UNAUTHORIZED =>
                    {
                        es.close();
                        return Err(CliError::Server(format!(
                            "not authorized for {server} — run `horsie auth login --server {server}`"
                        )));
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

/// Probe for resume: only the cursor matters. Deliberately NOT the full
/// `MessageFrame` — a line from an older/newer schema still yields its id.
#[derive(serde::Deserialize)]
struct CursorProbe {
    id: Option<String>,
}

/// Last appended message id written to `path`, scanning forward with a
/// buffered reader (no whole-file load, no partial-line edge cases). Absent
/// file → `Ok(None)`, meaning tail from the transcript's beginning.
fn scan_last_message_id(path: &Path) -> Result<Option<String>, CliError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CliError::Io(format!("read {}: {e}", path.display()))),
    };
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| CliError::Io(format!("read {}: {e}", path.display())))?;
        if let Ok(probe) = serde_json::from_str::<CursorProbe>(&line)
            && probe.id.is_some()
        {
            last = probe.id;
        }
    }
    Ok(last)
}

/// `horsie session list` — every session the server knows about.
pub async fn list(server: &str) -> Result<(), CliError> {
    let sessions = ServerClient::new(server).await?.list_sessions().await?;
    print!("{}", render_session_table(&sessions, now_ms()));
    Ok(())
}

/// `horsie session status <id>` — a point-in-time snapshot (live progress is
/// `session tail`'s job).
pub async fn status(server: &str, session_id: &str) -> Result<(), CliError> {
    let detail = ServerClient::new(server)
        .await?
        .get_session(session_id)
        .await?;
    print!("{}", render_session_detail(&detail, now_ms()));
    Ok(())
}

/// "just now", "5m ago", "3h ago", "2d ago".
pub(crate) fn relative(now_ms: u64, then_ms: u64) -> String {
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
    // of them are answered.
    {
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
    use horsie_models::agent::{AgentLogBody, AgentLogEntry, Message};
    use horsie_models::session::MessageDelta;

    #[test]
    fn output_path_inside_existing_dir_uses_session_id_filename() {
        let dir = tempfile::tempdir().unwrap();
        let p = output_path(dir.path(), "abc-123");
        assert_eq!(p, dir.path().join("abc-123.jsonl"));
    }

    #[test]
    fn output_path_for_plain_file_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tail.jsonl");
        assert_eq!(output_path(&file, "abc-123"), file);
    }

    fn sink(mode: EventsMode, cursor: Option<String>) -> (SessionSink, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.jsonl");
        let out = open_append(&path).unwrap();
        (SessionSink { out, mode, cursor }, dir, path)
    }

    fn lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn entry(seq: u64) -> String {
        serde_json::to_string(&MessageFrame::Entry(AgentLogEntry {
            seq,
            at_ms: 0,
            body: AgentLogBody::Llm(Message::user(&format!("m{seq}"), "hi", 0)),
        }))
        .unwrap()
    }

    fn delta(entry_seq: u64, delta_seq: u32) -> String {
        serde_json::to_string(&MessageFrame::Delta(MessageDelta {
            entry_seq,
            delta_seq,
            text: "chunk".into(),
            reset: false,
        }))
        .unwrap()
    }

    #[test]
    fn an_entry_is_written_with_its_seq_as_the_resume_cursor() {
        let (mut s, _dir, path) = sink(EventsMode::All, None);
        assert!(s.handle("7", &entry(7)).unwrap());
        s.flush().unwrap();
        let written = lines(&path);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["id"], serde_json::json!("7"));
        assert_eq!(written[0]["event"]["type"], serde_json::json!("Entry"));
        assert_eq!(s.cursor().as_deref(), Some("7"));
    }

    /// The change the two-part cursor buys: a reconnect mid-message resumes
    /// inside it rather than replaying the whole thing, because a delta has a
    /// position of its own.
    #[test]
    fn a_delta_advances_the_cursor_into_the_entry_it_follows() {
        let (mut s, _dir, path) = sink(EventsMode::All, Some("7".into()));
        assert!(s.handle("7.3", &delta(7, 3)).unwrap());
        s.flush().unwrap();
        assert_eq!(lines(&path).len(), 1);
        assert_eq!(s.cursor().as_deref(), Some("7.3"));
    }

    #[test]
    fn messages_mode_skips_deltas_but_still_tracks_where_it_is() {
        let (mut s, _dir, path) = sink(EventsMode::Messages, Some("7".into()));
        assert!(!s.handle("7.1", &delta(7, 1)).unwrap());
        s.flush().unwrap();
        assert!(lines(&path).is_empty(), "a delta is not a message");
        // Skipped from the file, not from the position: a resume must not
        // re-receive frames this tail deliberately dropped.
        assert_eq!(s.cursor().as_deref(), Some("7.1"));
    }

    #[test]
    fn an_unparseable_frame_skips_ahead_on_the_streams_own_id() {
        let (mut s, _dir, path) = sink(EventsMode::All, Some("7".into()));
        assert!(!s.handle("8", "{not json").unwrap());
        s.flush().unwrap();
        assert!(lines(&path).is_empty());
        assert_eq!(
            s.cursor().as_deref(),
            Some("8"),
            "trust the stream's id rather than re-receiving a frame that cannot be read"
        );
    }
}
