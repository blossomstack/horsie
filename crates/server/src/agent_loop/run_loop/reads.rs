//! Questions answered from state, which wake nothing.
//!
//! No journal access, no run, no turn boundary. A read is the one thing an
//! agent does that cannot change it, which is why these arrive as their own
//! command group rather than as special cases inside the ones that can.
//!
//! [`AgentState::read_from`] is the exception worth reading: it answers with
//! durable entries *plus*, when the caller has caught up to the tail, the
//! deltas of the message still being written. Those two travel together because
//! separating them would let a client hold a delta that belongs after an entry
//! it has not seen.

use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;
use horsie_agentcore::AgentLogEntry;
use serde::{Deserialize, Serialize};

/// What a live reader gets for one step forward.
///
/// Entries and deltas are answered together because they are two halves of one
/// position, and separating them would let a client hold a delta that belongs
/// after an entry it has not seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadOutcome {
    /// Set only on a cursorless read: what the replayed window covers, and
    /// whether the cap left anything behind. A resuming caller already knows
    /// where it is, so it gets `None`.
    pub window: Option<ReplayWindow>,
    /// Durable entries after the caller's `entry_seq`.
    pub entries: Vec<AgentLogEntry>,
    /// Chunks of the message still being written, after the caller's
    /// `delta_seq`. Empty when the caller is behind the tail — live typing
    /// means nothing to a reader that has not caught up to the entry it
    /// follows.
    pub deltas: Vec<String>,
    /// The caller's delta position is impossible for the run now in flight, so
    /// it was talking to one that has since restarted. `deltas` therefore
    /// starts from the beginning and the caller must discard what it holds.
    pub reset_deltas: bool,
    /// Where the caller now is.
    pub cursor: crate::agent_loop::shared::agent_log::Cursor,
}

/// What a cursorless replay covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub has_more_before: bool,
    pub earliest_seq: Option<u64>,
}

impl ReadOutcome {
    /// Nothing new — the reader is exactly where the agent is.
    pub(crate) fn nothing(cursor: crate::agent_loop::shared::agent_log::Cursor) -> Self {
        Self {
            window: None,
            entries: Vec::new(),
            deltas: Vec::new(),
            reset_deltas: false,
            cursor,
        }
    }

    /// Whether this outcome is worth sending. A wakeup can lose a race with
    /// another reader and find nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.deltas.is_empty() && !self.reset_deltas
    }
}

impl Transcript {
    /// Answer a forward read from `after`, against the deltas now in flight.
    ///
    /// Three cases, and the third is the whole reason the cursor has two parts:
    ///
    /// - **Behind the tail.** Entries only. Live typing means nothing to a
    ///   reader that has not reached the entry those deltas follow, and sending
    ///   them would place chunks of a message above messages it comes after.
    /// - **Caught up.** The deltas past the caller's own position.
    /// - **Claiming more deltas than exist.** Impossible for the run now in
    ///   flight, so the caller was talking to one that has since restarted —
    ///   entry `N` is still the tail after a crash, but the new run starts its
    ///   deltas again from one. Answered with everything and a reset. A single
    ///   flat counter would have reissued the same numbers to different content
    ///   and nothing could have noticed.
    #[must_use]
    pub fn read_from(
        &self,
        after: Option<crate::agent_loop::shared::agent_log::Cursor>,
        deltas: &[String],
    ) -> ReadOutcome {
        let tail = self.tail_seq();
        let Some(cursor) = after else {
            // No position at all: the newest window, capped. A long-running
            // session must not resend its whole history on every open, and the
            // caller is told when the cap bit so it can page back for the rest.
            let (entries, truncated) =
                crate::agent_loop::shared::agent_log::replay_window(self.entries());
            return ReadOutcome {
                window: Some(ReplayWindow {
                    has_more_before: truncated,
                    earliest_seq: entries.first().map(|e| e.seq),
                }),
                entries: entries.to_vec(),
                deltas: deltas.to_vec(),
                reset_deltas: false,
                cursor: crate::agent_loop::shared::agent_log::Cursor {
                    entry_seq: tail.unwrap_or(0),
                    delta_seq: deltas.len(),
                },
            };
        };

        let entries =
            crate::agent_loop::shared::agent_log::since(self.entries(), cursor.entry_seq).to_vec();
        if !entries.is_empty() {
            // Behind the tail. The deltas belong after the entries this reader
            // is only now receiving, so they wait for the next step.
            return ReadOutcome {
                window: None,
                cursor: crate::agent_loop::shared::agent_log::Cursor {
                    entry_seq: entries.last().map_or(cursor.entry_seq, |e| e.seq),
                    delta_seq: 0,
                },
                entries,
                deltas: Vec::new(),
                reset_deltas: false,
            };
        }

        if cursor.delta_seq > deltas.len() {
            return ReadOutcome {
                window: None,
                entries: Vec::new(),
                deltas: deltas.to_vec(),
                reset_deltas: true,
                cursor: crate::agent_loop::shared::agent_log::Cursor {
                    entry_seq: cursor.entry_seq,
                    delta_seq: deltas.len(),
                },
            };
        }

        if cursor.delta_seq == deltas.len() {
            return ReadOutcome::nothing(cursor);
        }

        ReadOutcome {
            window: None,
            entries: Vec::new(),
            deltas: deltas[cursor.delta_seq..].to_vec(),
            reset_deltas: false,
            cursor: crate::agent_loop::shared::agent_log::Cursor {
                entry_seq: cursor.entry_seq,
                delta_seq: deltas.len(),
            },
        }
    }
}

/// Answer a read from in-memory state without waking the agent.
pub(crate) async fn query(
    cmd: QueryCommand,
    cx: &mut CommandContext<'_>,
) -> CommandEffect<AgentDomainEvent> {
    let state = cx.state;
    match cmd {
        QueryCommand::ReadLog { after, reply } => {
            let transcript = state.transcript();
            let _ = reply.send(transcript.read_from(after, &cx.step_run.streamed_text));
            CommandEffect::none()
        }
        QueryCommand::PageLog {
            anchor,
            max,
            filter,
            reply,
        } => {
            let _ = reply.send(crate::agent_loop::shared::agent_log::page(
                state.transcript().entries(),
                anchor,
                max,
                &filter,
            ));
            CommandEffect::none()
        }
        QueryCommand::SearchLog {
            needle,
            max,
            filter,
            reply,
        } => {
            let _ = reply.send(crate::agent_loop::shared::agent_log::search(
                state.transcript().entries(),
                &needle,
                &filter,
                max,
            ));
            CommandEffect::none()
        }
        QueryCommand::SeqOfId { id, reply } => {
            let _ = reply.send(crate::agent_loop::shared::agent_log::seq_of_id(
                state.transcript().entries(),
                &id,
            ));
            CommandEffect::none()
        }
        QueryCommand::CanOffload { reply } => {
            let safe =
                !cx.step_run.is_running() && !state.turn_in_flight() && state.open_step().is_none();
            let _ = reply.send(safe);
            CommandEffect::none()
        }
        QueryCommand::GetUsage { reply } => {
            let _ = reply.send(state.usage_snapshot());
            CommandEffect::none()
        }
        QueryCommand::GetState { reply } => {
            let _ = reply.send(state.state_view());
            CommandEffect::none()
        }
        QueryCommand::LogHead { reply } => {
            let _ = reply.send(state.next_seq());
            CommandEffect::none()
        }
    }
}
