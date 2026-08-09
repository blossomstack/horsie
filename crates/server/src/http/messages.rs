//! `GET /api/sessions/:id/messages` — the one read.
//!
//! Replaces three endpoints: a paged `/history`, a per-agent SSE stream, and a
//! per-session SSE stream. They were three because a transcript, an agent's
//! live frames, and session-scoped current values were three different sources.
//! They are one now because the agent's log is the only source: the session
//! actor tells an agent to record what happened to it, so a viewer reads one
//! ordered thing instead of reconciling three.
//!
//! **Nothing is pushed at a reader.** The agent publishes its position — a
//! `(tail_seq, delta_count)` pair — into a `watch`, and each connection reads
//! forward from its own cursor when that changes. A `watch` keeps only the
//! latest value and overwrites it, so a slow reader cannot fall behind it; it
//! simply sees a larger jump when it next looks. That is why there is no
//! backfill loop here, no `Resync` frame, and no capacity constant to tune —
//! the overflow those existed to handle cannot occur.

use crate::agent_loop::Cursor;
use crate::http::Scope;
use crate::http::error::Api;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use horsie_models::session::{MessageDelta, MessageFrame, MessageWindow};
use horsie_models::session_api::MessagesPage;
use serde::Deserialize;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// Channel depth for one connection's outbound frames.
///
/// Unlike the broadcast capacity this replaces, overrunning it costs nothing:
/// the writer simply awaits, the reader's cursor stops advancing, and it
/// catches up on the next look. Nothing is dropped and nobody is told to
/// re-sync, because the data was never in here to lose.
const STREAM_BUFFER: usize = 64;

/// Default and maximum entries in one page.
const PAGE_DEFAULT: usize = 50;
const PAGE_MAX: usize = 1000;

/// The agent every request means unless it names another.
const MAIN_AGENT: &str = "main";

#[derive(Deserialize)]
pub struct MessagesParams {
    /// Which agent's log. Absent or `"main"` for the session's primary agent,
    /// else a subagent or workflow-step agent id.
    aid: Option<String>,
    /// Stream: start after this cursor. Ignored when `before` is present.
    after: Option<String>,
    /// Page: return the entries immediately before this seq. Its presence is
    /// what selects the page form — there is no mode flag and no `Accept`
    /// negotiation.
    before: Option<u64>,
    /// Page size. Only meaningful with `before`.
    max: Option<usize>,
}

/// The `Last-Event-ID` a reconnecting browser sends back.
fn last_event_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub async fn read_messages(
    Scope(state): Scope,
    Path(id): Path<String>,
    Query(params): Query<MessagesParams>,
    headers: HeaderMap,
) -> Result<Response, Api> {
    // Parsed only to reject a malformed id before it reaches the supervisor.
    Uuid::parse_str(&id).map_err(|_| Api::not_found("no such session"))?;
    let agent_id = params.aid.unwrap_or_else(|| MAIN_AGENT.to_string());

    if let Some(before) = params.before {
        return page(&state, id, agent_id, Some(before), params.max).await;
    }
    // `max` alone is still a page — "the latest N" — just with no anchor.
    if params.after.is_none() && params.max.is_some() {
        return page(&state, id, agent_id, None, params.max).await;
    }

    // A malformed cursor is treated as absent, which re-seeds the reader from
    // the beginning rather than silently rewinding it to entry zero and
    // pretending that was where it asked to start.
    let after = params
        .after
        .or_else(|| last_event_id(&headers))
        .and_then(|raw| Cursor::parse(&raw));

    stream(state, id, agent_id, after).await
}

/// The page form: returns and closes.
async fn page(
    state: &crate::users::UserServices,
    id: String,
    agent_id: String,
    before: Option<u64>,
    max: Option<usize>,
) -> Result<Response, Api> {
    let max = max.unwrap_or(PAGE_DEFAULT).clamp(1, PAGE_MAX);
    let page = state
        .supervisor
        .ask(|reply| SessionSupervisorCommand::PageLog {
            id: id.clone(),
            agent_id: Some(agent_id),
            before,
            max,
            reply,
        })
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))?
        .ok_or_else(|| Api::not_found("no such agent"))?;
    let mut entries = page.entries;
    // Thinking signatures are provider-replay artifacts — kilobytes each, and
    // meaningless to a client. They stay in state and in the journal, never on
    // a wire.
    crate::wire_redact::strip_entry_signatures(&mut entries);
    // No `has_more`. Fewer entries than asked for means there are no more,
    // which says the same thing without a second way to say it.
    Ok(Json(MessagesPage { entries }).into_response())
}

/// The stream form: everything after the cursor, then live.
async fn stream(
    state: std::sync::Arc<crate::users::UserServices>,
    id: String,
    agent_id: String,
    after: Option<Cursor>,
) -> Result<Response, Api> {
    // Watch first, read second. Subscribing before the first read is what
    // closes the gap a read-then-subscribe would leave exactly the width of the
    // connect: anything appended in between is already reflected in the
    // position, so the first wakeup finds it.
    let mut position = state
        .supervisor
        .ask(|reply| SessionSupervisorCommand::WatchAgent {
            id: id.clone(),
            agent_id: Some(agent_id.clone()),
            reply,
        })
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))?
        .ok_or_else(|| Api::not_found("no such agent"))?;

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(STREAM_BUFFER);
    tokio::spawn(async move {
        let mut cursor = after;
        // A resuming reader already knows what window it is in; only a
        // cursorless one is owed the announcement, and only once.
        let mut announced = after.is_some();
        loop {
            let out = state
                .supervisor
                .ask(|reply| SessionSupervisorCommand::ReadLog {
                    id: id.clone(),
                    agent_id: Some(agent_id.clone()),
                    after: cursor,
                    reply,
                })
                .await
                .ok()
                .flatten();
            let Some(out) = out else {
                // The agent is gone. Close the stream; the browser reconnects
                // and gets a fresh read, which is the same path a dropped
                // connection already takes.
                return;
            };

            // The window frame, when there is one, precedes the entries it
            // describes — a client needs to know whether it is looking at the
            // whole log before it decides anything about the top of it.
            if let Some(window) = out.window.as_ref().filter(|_| !announced) {
                announced = true;
                let frame = MessageFrame::Window(MessageWindow {
                    has_more_before: window.has_more_before,
                    earliest_seq: window.earliest_seq,
                });
                match Event::default().json_data(frame) {
                    Ok(ev) => {
                        if tx.send(Ok(ev)).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to serialize the window frame");
                    }
                }
            }

            let advanced = !out.is_empty();
            let mut entries = out.entries;
            crate::wire_redact::strip_entry_signatures(&mut entries);
            for entry in &entries {
                let ev = Event::default()
                    .id(entry.seq.to_string())
                    .json_data(MessageFrame::Entry(entry.clone()));
                match ev {
                    Ok(ev) => {
                        if tx.send(Ok(ev)).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to serialize a log entry; skipping");
                    }
                }
            }
            // Deltas number from the caller's own position, so the ids they
            // carry continue where that reader left off rather than from one.
            let base = if out.reset_deltas {
                0
            } else {
                cursor.map_or(0, |c| c.delta_seq)
            };
            for (offset, text) in out.deltas.iter().enumerate() {
                let delta_seq = base + offset + 1;
                let frame = MessageFrame::Delta(MessageDelta {
                    entry_seq: out.cursor.entry_seq,
                    delta_seq: u32::try_from(delta_seq).unwrap_or(u32::MAX),
                    text: text.clone(),
                    reset: out.reset_deltas && offset == 0,
                });
                let id = Cursor {
                    entry_seq: out.cursor.entry_seq,
                    delta_seq,
                }
                .to_string();
                match Event::default().id(id).json_data(frame) {
                    Ok(ev) => {
                        if tx.send(Ok(ev)).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to serialize a delta; skipping");
                    }
                }
            }
            // Only advance on something actually received. An empty log would
            // otherwise leave the cursor claiming entry 0 — a position it does
            // not hold — and `page_after(log, 0)` skips seq 0, so the session's
            // very first entry would never be sent.
            if advanced {
                cursor = Some(out.cursor);
            }

            // Nothing new, so wait to be told there is. `changed()` also
            // returns immediately if the position moved while we were writing,
            // which is what keeps a fast producer from outrunning a slow
            // reader without any of them losing data.
            if !advanced && position.changed().await.is_err() {
                return;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}
