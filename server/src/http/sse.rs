//! SSE streams: one per session (`/api/sessions/:id/events`), one per agent
//! (`/api/sessions/:id/agents/:agent_id/events`), and a global session-list feed
//! (`/api/events`).
//!
//! Only an agent stream's `Appended` frames carry an SSE `id:`, and that id is
//! the message id — the same cursor `/history` pages with. On reconnect the
//! browser sends it back as `Last-Event-ID` and the server serves the gap from
//! the agent's in-memory state. Nothing here reads a journal.
//!
//! Everything else is either a current value (status, inbox, task list, usage)
//! or ephemeral run noise (deltas, tool starts). Neither is a log, so neither
//! gets a cursor: a client that missed one re-reads the document.

use crate::http::Scope;
use crate::http::error::Api;
use crate::http::handlers::{wire_queued_message, wire_status_kind};
use crate::sessions::events::{resync_frame, wire_agent_frame};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::{AgentFrame, SessionFrame};
use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use horsie_models::session::{
    AgentTreeEvent, ErrorEvent, InboxChangedEvent, ProgressionEvent, SessionEvent,
    StatusChangedEvent,
};
use std::convert::Infallible;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// Channel depth for one SSE connection's outbound frames.
const STREAM_BUFFER: usize = 64;

/// Pages of backfill a reconnecting stream will fetch before giving up and
/// telling the client to re-sync. A bound, not a limit anyone should hit: it
/// only trips if a session appends faster than the backfill drains.
const MAX_BACKFILL_PAGES: usize = 100;

/// Messages per backfill page.
const BACKFILL_LIMIT: usize = 200;

/// The `Last-Event-ID` cursor: a message id, not a number. Absent → the client
/// has no position and backfills through `/history` instead.
fn last_event_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Serialize a frame, logging and skipping one that cannot be encoded.
fn encode<T: serde::Serialize>(id: Option<&str>, payload: &T) -> Option<Event> {
    let event = match id {
        Some(id) => Event::default().id(id),
        None => Event::default(),
    };
    match event.json_data(payload) {
        Ok(e) => Some(e),
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize SSE event; skipping");
            None
        }
    }
}

/// One agent's stream: durable appends (with ids), current values, and live run
/// noise. Reconnect resumes from `Last-Event-ID` — served from the agent's own
/// state, never its journal.
pub async fn agent_events(
    Scope(state): Scope,
    Path((id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Api> {
    Uuid::parse_str(&id).map_err(|_| Api::not_found("no such session"))?;
    let mut sub = state
        .supervisor
        .ask(|reply| SessionSupervisorCommand::SubscribeAgent {
            id: id.clone(),
            agent_id: Some(agent_id.clone()),
            reply,
        })
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))?
        .ok_or_else(|| Api::not_found("no such agent"))?;

    let cursor = last_event_id(&headers);
    let supervisor = state.supervisor.clone();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(STREAM_BUFFER);

    tokio::spawn(async move {
        // 1) Backfill everything appended since the client's cursor. Subscribing
        //    happened first, so anything appended *during* the backfill is
        //    already queued in the broadcast and cannot be lost — at worst it
        //    arrives twice, and an append is idempotent by message id.
        if let Some(cursor) = cursor {
            let mut at = cursor;
            for _ in 0..MAX_BACKFILL_PAGES {
                let page = supervisor
                    .ask(|reply| SessionSupervisorCommand::History {
                        id: id.clone(),
                        agent_id: Some(agent_id.clone()),
                        query: horsie_workflow::HistoryQuery {
                            before: None,
                            after: Some(at.clone()),
                            limit: BACKFILL_LIMIT,
                        },
                        reply,
                    })
                    .await
                    .ok()
                    .flatten();
                let Some(page) = page else { break };
                for entry in page.entries {
                    at = entry.id().to_string();
                    let frame = wire_agent_frame(AgentFrame::Appended { entry });
                    if let Some(ev) = encode(Some(&at), &frame)
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                if !page.has_more_after {
                    break;
                }
            }
        }

        // 2) Live loop.
        loop {
            match sub.recv().await {
                Ok(frame) => {
                    // Only an append is a log entry, so only an append gets an id.
                    let id = match &frame {
                        AgentFrame::Appended { entry } => Some(entry.id().to_string()),
                        AgentFrame::Delta { .. }
                        | AgentFrame::ToolStart { .. }
                        | AgentFrame::TurnCompleted { .. }
                        | AgentFrame::TaskListChanged { .. } => None,
                    };
                    let wire = wire_agent_frame(frame);
                    if let Some(ev) = encode(id.as_deref(), &wire)
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                // Frames were dropped. Say so instead of replaying: a live
                // stream is not a log, and the client already knows how to
                // backfill from its cursor.
                Err(RecvError::Lagged(_)) => {
                    if let Some(ev) = encode(None, &resync_frame())
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

/// The session stream: session-scoped current values only. No transcript (that
/// belongs to an agent), no ids, no cursor — a client that misses a frame
/// re-reads the session document.
pub async fn session_events(
    Scope(state): Scope,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Api> {
    // Parsed only to reject a malformed id before it reaches the supervisor.
    Uuid::parse_str(&id).map_err(|_| Api::not_found("no such session"))?;
    let mut sub = state
        .supervisor
        .ask(|reply| SessionSupervisorCommand::Subscribe {
            id: id.clone(),
            reply,
        })
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))?
        .ok_or_else(|| Api::not_found("no such session"))?;

    // Subscribed, so nothing further can be missed — now ask for the queue as
    // it already stands. The answer comes back through the same broadcast,
    // which is what orders it against live frames: anything sent before it is
    // older and superseded, anything after is newer. Reading the queue here and
    // writing it into `tx` instead would put a snapshot ahead of frames that
    // predate it, which is the exact shape of bug this replaces.
    let _ = state
        .supervisor
        .tell(SessionSupervisorCommand::PublishInbox { id: id.clone() })
        .await;

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(STREAM_BUFFER);
    tokio::spawn(async move {
        loop {
            let frame = match sub.recv().await {
                Ok(frame) => frame,
                // Every frame here is a whole current value, so a dropped one is
                // superseded by the next. Nothing to recover.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let event = match frame {
                SessionFrame::Status { status } => {
                    SessionEvent::StatusChanged(StatusChangedEvent {
                        status: wire_status_kind(&status),
                        reason: crate::sessions::spec::status_reason(&status),
                        // A park says what it is waiting for, so a watching client
                        // can answer without refetching the session.
                        pending_asks: crate::http::handlers::wire_pending_asks(&status),
                    })
                }
                SessionFrame::InboxChanged { queued } => {
                    SessionEvent::InboxChanged(InboxChangedEvent {
                        queued: queued.into_iter().map(wire_queued_message).collect(),
                    })
                }
                SessionFrame::Error { message } => SessionEvent::Error(ErrorEvent { message }),
                SessionFrame::Progression {
                    stage,
                    detail,
                    at_ms,
                } => SessionEvent::Progressed(ProgressionEvent {
                    stage,
                    detail,
                    at_ms,
                }),
                // The roster itself rides the session document; this only says
                // "it changed", which is all a client needs to re-read it.
                SessionFrame::AgentTreeChanged => {
                    SessionEvent::AgentTreeChanged(AgentTreeEvent { agents: Vec::new() })
                }
            };
            if let Some(ev) = encode(None, &event)
                && tx.send(Ok(ev)).await.is_err()
            {
                return;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

pub async fn global_events(
    Scope(state): Scope,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut sub = state.global_events.subscribe();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        loop {
            match sub.recv().await {
                Ok(frame) => match Event::default().json_data(&frame) {
                    Ok(ev) => {
                        if tx.send(Ok(ev)).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to serialize global SSE event");
                    }
                },
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
