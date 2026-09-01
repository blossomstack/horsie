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

use super::*;
use horsie_actor::{ActorContext, CommandEffect};
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
    pub cursor: crate::agent_loop::agent_log::Cursor,
}

/// What a cursorless replay covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub has_more_before: bool,
    pub earliest_seq: Option<u64>,
}

impl ReadOutcome {
    /// Nothing new — the reader is exactly where the agent is.
    pub(super) fn nothing(cursor: crate::agent_loop::agent_log::Cursor) -> Self {
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

impl AgentState {
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
        after: Option<crate::agent_loop::agent_log::Cursor>,
        deltas: &[String],
    ) -> ReadOutcome {
        let tail = self.tail_seq();
        let Some(cursor) = after else {
            // No position at all: the newest window, capped. A long-running
            // session must not resend its whole history on every open, and the
            // caller is told when the cap bit so it can page back for the rest.
            let (entries, truncated) = crate::agent_loop::agent_log::replay_window(&self.log);
            return ReadOutcome {
                window: Some(ReplayWindow {
                    has_more_before: truncated,
                    earliest_seq: entries.first().map(|e| e.seq),
                }),
                entries: entries.to_vec(),
                deltas: deltas.to_vec(),
                reset_deltas: false,
                cursor: crate::agent_loop::agent_log::Cursor {
                    entry_seq: tail.unwrap_or(0),
                    delta_seq: deltas.len(),
                },
            };
        };

        let entries = crate::agent_loop::agent_log::since(&self.log, cursor.entry_seq).to_vec();
        if !entries.is_empty() {
            // Behind the tail. The deltas belong after the entries this reader
            // is only now receiving, so they wait for the next step.
            return ReadOutcome {
                window: None,
                cursor: crate::agent_loop::agent_log::Cursor {
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
                cursor: crate::agent_loop::agent_log::Cursor {
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
            cursor: crate::agent_loop::agent_log::Cursor {
                entry_seq: cursor.entry_seq,
                delta_seq: deltas.len(),
            },
        }
    }
}

/// Questions answered from state, which wake nothing.
pub(super) struct Reads;

impl Reads {
    pub(super) async fn handle(
        actor: &mut AgentActor,
        state: &AgentState,
        cmd: ReadCommand,
        _ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            ReadCommand::ReadLog { after, reply } => {
                let _ = reply.send(state.read_from(after, &actor.deltas));
                CommandEffect::none()
            }
            ReadCommand::PageLog {
                anchor,
                max,
                filter,
                reply,
            } => {
                let _ = reply.send(crate::agent_loop::agent_log::page(
                    &state.log, anchor, max, &filter,
                ));
                CommandEffect::none()
            }
            ReadCommand::SearchLog {
                needle,
                max,
                filter,
                reply,
            } => {
                let _ = reply.send(crate::agent_loop::agent_log::search(
                    &state.log, &needle, &filter, max,
                ));
                CommandEffect::none()
            }
            ReadCommand::SeqOfId { id, reply } => {
                let _ = reply.send(crate::agent_loop::agent_log::seq_of_id(&state.log, &id));
                CommandEffect::none()
            }
            ReadCommand::GetUsage { reply } => {
                let _ = reply.send(state.usage_snapshot());
                CommandEffect::none()
            }
            ReadCommand::GetState { reply } => {
                let _ = reply.send(state.state_view());
                CommandEffect::none()
            }
            ReadCommand::LogHead { reply } => {
                let _ = reply.send(state.next_seq);
                CommandEffect::none()
            }
        }
    }
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
    use crate::agent_loop::agent_actor::testing::*;
    use horsie_agentcore::{AgentLogBody, Message};
    fn log_upto(n: u64) -> AgentState {
        (0..n).fold(AgentActor::initial_state(), |state, i| {
            AgentActor::apply_event(
                state,
                AgentDomainEvent::MessageComplete {
                    message: Message::user(format!("m{i}"), "x", i),
                },
            )
        })
    }

    fn chunks(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| (*s).to_string()).collect()
    }

    /// Live typing means nothing to a reader that has not reached the entry
    /// those chunks follow — sending them would draw fragments of a message
    /// above messages it comes after.
    #[test]
    fn a_reader_behind_the_tail_gets_entries_and_no_deltas() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_loop::agent_log::Cursor {
                entry_seq: 1,
                delta_seq: 0,
            }),
            &chunks(&["x", "y"]),
        );
        assert_eq!(
            out.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(out.deltas.is_empty());
        assert_eq!(out.cursor.entry_seq, 4);
        assert_eq!(out.cursor.delta_seq, 0);
    }

    #[test]
    fn a_caught_up_reader_gets_the_deltas_after_its_own() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_loop::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 1,
            }),
            &chunks(&["x", "y", "z"]),
        );
        assert!(out.entries.is_empty());
        assert_eq!(out.deltas, vec!["y", "z"]);
        assert!(!out.reset_deltas);
        assert_eq!(out.cursor.delta_seq, 3);
    }

    /// The trap the two-part cursor exists to close.
    ///
    /// Entry 4 is still the tail after a crash, but the run that emitted the
    /// reader's 50 deltas is gone and the new one has emitted two. `50 > 2` is
    /// impossible for a live run, so the mismatch is arithmetic — and a single
    /// flat counter would have reissued 5..55 to different content with nothing
    /// able to notice.
    #[test]
    fn a_restarted_run_is_detected_and_answered_with_a_reset() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_loop::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 50,
            }),
            &chunks(&["a", "b"]),
        );
        assert!(
            out.reset_deltas,
            "50 deltas cannot precede a run that has 2"
        );
        assert_eq!(out.deltas, vec!["a", "b"]);
        assert_eq!(out.cursor.delta_seq, 2);
    }

    #[test]
    fn a_reader_exactly_at_the_position_gets_nothing() {
        let state = log_upto(5);
        let out = state.read_from(
            Some(crate::agent_loop::agent_log::Cursor {
                entry_seq: 4,
                delta_seq: 2,
            }),
            &chunks(&["a", "b"]),
        );
        assert!(out.is_empty(), "a wakeup that lost a race sends nothing");
    }

    #[test]
    fn a_reader_with_no_cursor_gets_everything() {
        let state = log_upto(3);
        let out = state.read_from(None, &chunks(&["a"]));
        assert_eq!(
            out.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(out.deltas, vec!["a"]);
        assert_eq!(out.cursor.entry_seq, 2);
        assert_eq!(out.cursor.delta_seq, 1);
    }

    /// State is snapshotted, so a mixed transcript has to survive a round trip
    /// through serde or recovery loses every hook record it ever wrote.
    #[test]
    fn a_mixed_transcript_survives_a_snapshot_round_trip() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = with_hook(state, "guard", "tc1", 0);

        let json = serde_json::to_string(&state).unwrap();
        let back: AgentState = serde_json::from_str(&json).unwrap();

        // Both halves of an entry have to survive: the id it joins on and the
        // seq it is ordered by. A snapshot that kept the bodies but lost the
        // numbering would leave every live cursor pointing at the wrong entry.
        let shape = |s: &AgentState| -> Vec<(u64, Option<String>)> {
            s.log
                .iter()
                .map(|e| (e.seq, e.body.id().map(str::to_string)))
                .collect()
        };
        assert_eq!(shape(&back), shape(&state));
        assert_eq!(back.next_seq, state.next_seq);
        match &back.log[1].body {
            AgentLogBody::Hook(h) => {
                assert_eq!(h.record.plugin, "guard");
                // The externally-tagged union has to survive the round trip,
                // outcome and all — a snapshot is what a recovered transcript
                // is rebuilt from.
                match &h.record.action {
                    horsie_models::hooks::HookAction::PreToolUse(r) => {
                        assert_eq!(r.call.tool_call_id, "tc1");
                        match &r.outcome {
                            horsie_models::hooks::PreToolUseOutcome::Denied(d) => {
                                assert_eq!(d.reason.as_deref(), Some("not allowed"));
                            }
                            other => panic!("expected a denial, got {other:?}"),
                        }
                    }
                    other => panic!("expected a PreToolUse action, got {other:?}"),
                }
            }
            other => panic!("expected a hook entry, got {other:?}"),
        }
    }
}
