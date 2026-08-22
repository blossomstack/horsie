//! The global session-list feed (`/api/events`).
//!
//! **Nothing is pushed at a reader**, which is the same shape `GET
//! /api/sessions/:id/messages` already had and for the same reason. The
//! supervisor moves a revision when a session's status, title or sub sessions
//! change; each connection asks it to say when that number last moved, then
//! reads the list. The counter keeps only its latest value, so a slow reader
//! cannot fall behind it — it simply sees a larger jump when it next looks.
//!
//! What that replaces is a per-account `broadcast::Sender` this module used to
//! subscribe to, and it fixes two things at once:
//!
//! 1. **It works across nodes.** The channel was a pointer into one process, so
//!    a reader whose connection landed on one node never heard about a session
//!    whose supervisor was on another. An `ask` reaches the supervisor wherever
//!    the cluster put it.
//! 2. **A slow reader can no longer go stale.** The old loop answered
//!    `RecvError::Lagged` by continuing, silently dropping the frames it missed
//!    — so a session's row stayed wrong until something else changed it. There
//!    is nothing to lag now.
//!
//! A frame carries the whole list rather than a per-field delta. The revision
//! says *that* the list moved, never what moved in it, and the list has no
//! ordered log to read forward from the way an agent's transcript does — so a
//! reader replaces what it has instead of patching it.

use crate::control::sessions::{ListSessions, list};
use crate::http::Scope;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Channel depth for one connection's outbound frames.
///
/// Overrunning it costs nothing, unlike the broadcast capacity it replaces: the
/// writer simply awaits, its next ask is made later, and the list it then reads
/// is the current one. Nothing is dropped, because nothing is queued that a
/// later read would not produce anyway.
const STREAM_BUFFER: usize = 16;

pub async fn global_events(
    Scope(state): Scope,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(STREAM_BUFFER);
    tokio::spawn(async move {
        // `None` on the first pass, which answers immediately with wherever the
        // list is — a fresh connection must not wait a poll window before its
        // first frame.
        let mut seen = None;
        loop {
            let Ok(revision) = state
                .supervisor
                .ask(|reply| SessionSupervisorCommand::AwaitListRevision { after: seen, reply })
                .await
            else {
                // The supervisor is unreachable — this node may have stood
                // down. End the stream; the client reconnects, and may land on
                // a node that can serve it.
                return;
            };
            if seen == Some(revision) {
                // The wait expired without a change. Say nothing and ask again,
                // rather than sending a frame that carries no news.
                continue;
            }
            seen = Some(revision);

            // The same projection the route answers with — filtered and
            // ordered identically, so a live replacement and a manual fetch
            // can never disagree.
            let Ok(body) = list(&state, ListSessions::default()).await else {
                return;
            };
            match Event::default().json_data(&body) {
                Ok(event) => {
                    if tx.send(Ok(event)).await.is_err() {
                        return; // the reader went away
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to serialize the session list");
                }
            }
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
