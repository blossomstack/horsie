//! The global session-list feed (`/api/events`).
//!
//! The per-session and per-agent streams that used to live here are gone. Both
//! are served by `GET /api/sessions/:id/messages`, which reads an agent's log
//! from its own state through a cursor rather than pushing frames at a
//! subscriber — so there is no backfill loop, no `Resync`, and no broadcast
//! capacity to overflow.

use crate::http::Scope;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use std::convert::Infallible;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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
