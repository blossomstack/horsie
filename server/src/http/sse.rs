//! SSE streams: one per session (`/api/sessions/:id/events`) and a global
//! session-list feed (`/api/events`).
//!
//! Durable coarse events carry an SSE `id:` equal to the agent-journal sequence
//! number — on reconnect, `Last-Event-ID` replays after that cursor, then the
//! stream bridges to the live broadcast. Ephemeral frames (deltas, tool starts,
//! status, errors) are sent live without an id.

use crate::http::AppState;
use crate::http::error::Api;
use crate::http::handlers::wire_status_kind;
use crate::sessions::SessionFrame;
use crate::sessions::events::{journal_head_seq, replay_session_events};
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use horsie_models::session::{
    DeltaEvent, ErrorEvent, ProgressionEvent, SessionEvent, StatusChangedEvent, ToolStartEvent,
};
use serde::Deserialize;
use std::convert::Infallible;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// Parse the `Last-Event-ID` header as a journal sequence cursor (default 0).
fn last_event_id(headers: &HeaderMap) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Build an id-stamped SSE event for a durable coarse event, or `None` if it
/// fails to serialize (logged, skipped).
fn stamped(seq: u64, event: &SessionEvent) -> Option<Event> {
    match Event::default().id(seq.to_string()).json_data(event) {
        Ok(e) => Some(e),
        Err(err) => {
            tracing::warn!(seq, error = %err, "failed to serialize SSE event; skipping");
            None
        }
    }
}

/// Build an id-less SSE event for a live frame.
fn live(event: &SessionEvent) -> Option<Event> {
    match Event::default().json_data(event) {
        Ok(e) => Some(e),
        Err(err) => {
            tracing::warn!(error = %err, "failed to serialize live SSE event; skipping");
            None
        }
    }
}

/// Query params for the session SSE stream.
#[derive(Deserialize)]
pub struct EventsParams {
    /// When set (`live=1`), stream only events that occur *after* connect —
    /// skipping the full journal replay. A paginating client backfills history
    /// via `/history` and uses this to receive just live updates.
    live: Option<u8>,
}

pub async fn session_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<EventsParams>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Api> {
    let sid = Uuid::parse_str(&id).map_err(|_| Api::not_found("no such session"))?;
    let sub = state
        .supervisor
        .ask(|reply| SessionSupervisorCommand::Subscribe {
            id: id.clone(),
            reply,
        })
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))?
        .ok_or_else(|| Api::not_found("no such session"))?;

    // `live=1` starts the cursor at the current journal head, so the replay step
    // emits nothing and only subsequent events stream. `Last-Event-ID` still wins
    // for reconnects (resume exactly after the last delivered id).
    let cursor = if params.live == Some(1) && headers.get("last-event-id").is_none() {
        journal_head_seq(&state.journal, sid).await
    } else {
        last_event_id(&headers)
    };
    let journal = state.journal.clone();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);

    tokio::spawn(async move {
        let mut last = cursor;
        // 1) Replay durable history after the cursor.
        for se in replay_session_events(&journal, sid, last).await {
            last = se.seq;
            if let Some(ev) = stamped(se.seq, &se.event)
                && tx.send(Ok(ev)).await.is_err()
            {
                return;
            }
        }
        // 2) Live loop.
        let mut sub = sub;
        loop {
            match sub.recv().await {
                // A coarse event was journaled (or we lagged) — re-read the
                // journal after our cursor to pick it up with stable ids.
                Ok(SessionFrame::Journaled) | Err(RecvError::Lagged(_)) => {
                    for se in replay_session_events(&journal, sid, last).await {
                        last = se.seq;
                        if let Some(ev) = stamped(se.seq, &se.event)
                            && tx.send(Ok(ev)).await.is_err()
                        {
                            return;
                        }
                    }
                }
                Ok(SessionFrame::Delta { text }) => {
                    if let Some(ev) = live(&SessionEvent::Delta(DeltaEvent { text }))
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                Ok(SessionFrame::ToolStart { tool_call_id, name }) => {
                    let se = SessionEvent::ToolStart(ToolStartEvent { tool_call_id, name });
                    if let Some(ev) = live(&se)
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                Ok(SessionFrame::Status { status }) => {
                    let se = SessionEvent::StatusChanged(StatusChangedEvent {
                        status: wire_status_kind(&status),
                        reason: crate::sessions::spec::status_reason(&status),
                    });
                    if let Some(ev) = live(&se)
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                Ok(SessionFrame::Error { message }) => {
                    if let Some(ev) = live(&SessionEvent::Error(ErrorEvent { message }))
                        && tx.send(Ok(ev)).await.is_err()
                    {
                        return;
                    }
                }
                Ok(SessionFrame::Progression {
                    stage,
                    detail,
                    at_ms,
                }) => {
                    let se = SessionEvent::Progressed(ProgressionEvent {
                        stage,
                        detail,
                        at_ms,
                    });
                    if let Some(ev) = live(&se)
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

pub async fn global_events(
    State(state): State<AppState>,
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
