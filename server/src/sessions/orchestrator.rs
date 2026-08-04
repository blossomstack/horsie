//! What a session does next.
//!
//! The decision is pure — no actors, no I/O, no clock — so it is unit-testable
//! against a hand-built [`SessionState`], and so a workflow run's sequencing is
//! a peer implementation rather than a branch inside the actor. The actor
//! performs whatever this returns; it never decides.

use crate::sessions::session_actor::{AgentKey, SessionState};
use crate::sessions::spec::SessionStatus;
use crate::sessions::subagents::SubAgentParent;
use horsie_models::agent::ToolResultInput;
use serde_json::Value;
use uuid::Uuid;

/// Separator between messages merged into one turn.
pub const MERGE_SEPARATOR: &str = "\n\n";

/// The tool result recorded for an ask the user walked away from.
pub const ABANDONED_ASK_RESULT: &str = "not answered — the user sent a new message instead";

/// Which command a caller is trying to run, for [`Orchestrator::accepts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCommandKind {
    UserMessage,
    Answer,
}

/// What an agent is resumed with.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub message: Option<String>,
    pub results: Vec<ToolResultInput>,
}

/// Something the actor should do. Every field is what the actor needs to
/// journal the action, so the actor never re-derives a decision.
#[derive(Debug, Clone)]
pub enum AgentAction {
    /// Begin one execution of one workflow step. Carries everything needed to
    /// both spawn the agent and journal the log entry, so the actor never
    /// re-derives a decision.
    StartStep {
        index: u32,
        step: String,
        agent: Uuid,
        attempt: u32,
        from: Option<u32>,
        via: Option<String>,
        input: String,
    },
    /// The run is over and succeeded, carrying the last step's output.
    Finish { output: Value },
    /// The run is over and failed.
    Fail { error: String },
    StartTurn {
        who: AgentKey,
        input: TurnInput,
        /// Inbox message ids this turn consumes.
        consumed: Vec<String>,
        /// Ask tool-call ids this turn answers.
        answered: Vec<String>,
        /// Subagents whose results this turn delivers.
        notified: Vec<Uuid>,
        /// A subagent parent this turn puts back into `Running`. `None` marks
        /// the session's own turn, which reports `Running` instead.
        mark_running: Option<Uuid>,
    },
}

/// Decides what a session does next. Pure.
pub trait Orchestrator: Send + Sync {
    /// Everything startable right now, in the order it should be performed.
    /// Called at every turn boundary: a message arriving while idle, a turn
    /// ending, a stop, a subagent finishing.
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction>;

    /// Whether this session kind takes that command.
    fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str>;
}

/// A person or a routine talking to one resident main agent.
pub struct InteractiveOrchestrator;

impl Orchestrator for InteractiveOrchestrator {
    fn next_actions(&self, state: &SessionState) -> Vec<AgentAction> {
        let mut actions = wake_owed_parents(state);
        if let Some(turn) = main_turn(state) {
            actions.push(turn);
        }
        actions
    }

    fn accepts(&self, cmd: SessionCommandKind) -> Result<(), &'static str> {
        match cmd {
            SessionCommandKind::UserMessage | SessionCommandKind::Answer => Ok(()),
        }
    }
}

/// Wake every idle subagent parent whose children have results it has not been
/// sent. The main agent is excluded: its owed results merge into its next turn
/// in [`main_turn`].
fn wake_owed_parents(state: &SessionState) -> Vec<AgentAction> {
    let tree = state.mode.subagents();
    tree.owed_by_sub_parent()
        .into_iter()
        .filter(|(parent, _)| !tree.is_running(parent))
        .map(|(parent, owed)| AgentAction::StartTurn {
            who: AgentKey::Sub(parent),
            input: TurnInput {
                message: Some(
                    owed.iter()
                        .map(|(_, text)| text.as_str())
                        .collect::<Vec<_>>()
                        .join(MERGE_SEPARATOR),
                ),
                results: Vec::new(),
            },
            consumed: Vec::new(),
            answered: Vec::new(),
            notified: owed.iter().map(|(child, _)| *child).collect(),
            mark_running: Some(parent),
        })
        .collect()
}

/// The main agent's turn, if one is owed and no run is in flight.
fn main_turn(state: &SessionState) -> Option<AgentAction> {
    // Owed subagent results ride every turn the main agent starts; with an
    // empty inbox they can also *start* one, but only from Idle — never
    // answering a pending ask, never chasing a failure.
    let owed = state.mode.subagents().owed_for(SubAgentParent::Main);
    if state.inbox.is_empty() && (owed.is_empty() || state.status != SessionStatus::Idle) {
        return None;
    }
    if matches!(
        state.status,
        SessionStatus::Running | SessionStatus::Unrecoverable { .. }
    ) {
        return None;
    }
    // One user message, not several: Anthropic requires alternating roles, so
    // consecutive user turns are not portable. Provenance survives in the
    // `MessageQueued` events.
    let mut parts: Vec<&str> = state.inbox.iter().map(|m| m.text.as_str()).collect();
    parts.extend(owed.iter().map(|(_, text)| text.as_str()));
    Some(AgentAction::StartTurn {
        who: AgentKey::Main,
        input: TurnInput {
            message: Some(parts.join(MERGE_SEPARATOR)),
            // A message sent while the agent is waiting on questions abandons
            // them: "never mind, do this instead". Every parked call still gets
            // a result, so nothing dangles on the wire — answering them for
            // real goes through `Answer`, which requires all of them at once.
            results: state
                .pending_asks
                .iter()
                .filter_map(|ask| ask.tool_call_id.clone())
                .map(|tool_call_id| ToolResultInput {
                    tool_call_id,
                    output: ABANDONED_ASK_RESULT.to_string(),
                    is_error: true,
                })
                .collect(),
        },
        consumed: state.inbox.iter().map(|m| m.id.clone()).collect(),
        answered: Vec::new(),
        notified: owed.iter().map(|(child, _)| *child).collect(),
        mark_running: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::InboxMessage;
    use crate::sessions::spec::PendingAsk;

    fn with_inbox(texts: &[&str]) -> SessionState {
        let mut s = SessionState::default();
        for (i, t) in texts.iter().enumerate() {
            s.inbox.push(InboxMessage {
                id: format!("m{i}"),
                text: (*t).to_string(),
                at_ms: 0,
            });
        }
        s
    }

    fn only_turn(state: &SessionState) -> AgentAction {
        let mut actions = InteractiveOrchestrator.next_actions(state);
        assert_eq!(actions.len(), 1, "expected exactly one action");
        actions.remove(0)
    }

    #[test]
    fn an_empty_inbox_starts_nothing() {
        assert!(
            InteractiveOrchestrator
                .next_actions(&SessionState::default())
                .is_empty()
        );
    }

    #[test]
    fn a_queued_message_starts_one_turn_that_consumes_it() {
        let AgentAction::StartTurn {
            who,
            input,
            consumed,
            ..
        } = only_turn(&with_inbox(&["hello"]))
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(who, AgentKey::Main);
        assert_eq!(input.message.as_deref(), Some("hello"));
        assert_eq!(consumed, vec!["m0".to_string()]);
    }

    /// Anthropic requires alternating roles, so several queued messages merge
    /// into one user turn rather than becoming consecutive user messages.
    #[test]
    fn several_queued_messages_merge_into_one_turn() {
        let AgentAction::StartTurn {
            input, consumed, ..
        } = only_turn(&with_inbox(&["a", "b"]))
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.message.as_deref(), Some("a\n\nb"));
        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn a_running_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Running;
        assert!(InteractiveOrchestrator.next_actions(&s).is_empty());
    }

    #[test]
    fn an_unrecoverable_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Unrecoverable {
            reason: "gone".into(),
        };
        assert!(InteractiveOrchestrator.next_actions(&s).is_empty());
    }

    /// A message sent while the agent is parked on questions abandons them —
    /// every parked call still gets a result, so nothing dangles on the wire.
    #[test]
    fn a_message_during_a_park_abandons_the_asks() {
        let mut s = with_inbox(&["never mind"]);
        s.pending_asks.push(PendingAsk {
            tool_call_id: Some("call_1".into()),
            question: "which?".into(),
        });
        s.status = SessionStatus::AwaitingInput {
            asks: s.pending_asks.clone(),
        };
        let AgentAction::StartTurn { input, .. } = only_turn(&s) else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.results.len(), 1);
        assert_eq!(input.results[0].tool_call_id, "call_1");
        assert!(input.results[0].is_error);
    }

    #[test]
    fn an_interactive_session_takes_messages_and_answers() {
        assert!(
            InteractiveOrchestrator
                .accepts(SessionCommandKind::UserMessage)
                .is_ok()
        );
        assert!(
            InteractiveOrchestrator
                .accepts(SessionCommandKind::Answer)
                .is_ok()
        );
    }
}
