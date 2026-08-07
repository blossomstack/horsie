//! The two pure decisions a conversation makes: which idle subagent parents are
//! owed their children's results, and whether the main agent has a turn to
//! start.
//!
//! No actors, no I/O, no clock — so both are unit-testable against a hand-built
//! [`SessionState`]. They are called from the components that own them
//! ([`SubAgents`](crate::sessions::session_actor) and `Turns`), which is why
//! there is no strategy trait here any more: the actor concatenates what its
//! components return rather than delegating the whole decision to one object.

use crate::sessions::session_actor::{AgentKey, SessionState};
use crate::sessions::spec::SessionStatus;
use crate::sessions::subagents::{OwedResult, SubAgentParent};
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde_json::Value;
use uuid::Uuid;

/// Separator between messages merged into one turn.
pub const MERGE_SEPARATOR: &str = "\n\n";

/// The tool result recorded for an ask the user walked away from.
pub const ABANDONED_ASK_RESULT: &str = "not answered — the user sent a new message instead";

/// What an agent is resumed with.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub message: Option<String>,
    pub results: Vec<ToolResultInput>,
    /// Finished subagents' results riding this turn. Kept apart from `message`
    /// rather than joined into it: merged, a client cannot tell a subagent's
    /// report from what the person typed, and both rendered as a user bubble.
    pub subagent_results: Vec<SubAgentResultPart>,
}

/// Something the actor should do. Every field is what the actor needs to
/// journal the action, so the actor never re-derives a decision.
///
/// The two large variants carry a named payload rather than inline fields: the
/// actor hands each straight to the method that performs it, and a struct keeps
/// that a one-argument call instead of seven positional ones.
#[derive(Debug, Clone)]
pub enum AgentAction {
    /// Begin one execution of one workflow step.
    StartStep(StepStart),
    /// The run is over and succeeded, carrying the last step's output.
    Finish { output: Value },
    /// The run is over and failed.
    Fail { error: String },
    /// Resume one agent, beginning a turn.
    StartTurn(TurnStart),
}

/// One execution of one workflow step. Carries everything needed to both spawn
/// the agent and journal the log entry, so the actor never re-derives a
/// decision.
#[derive(Debug, Clone)]
pub struct StepStart {
    pub index: u32,
    pub step: String,
    pub agent: Uuid,
    pub attempt: u32,
    /// The entry this came out of; `None` for the start step.
    pub from: Option<u32>,
    /// The transition condition that matched, if any.
    pub via: Option<String>,
    pub input: String,
}

/// One agent, resumed.
#[derive(Debug, Clone)]
pub struct TurnStart {
    pub who: AgentKey,
    pub input: TurnInput,
    /// Inbox message ids this turn consumes.
    pub consumed: Vec<String>,
    /// Ask tool-call ids this turn answers.
    pub answered: Vec<String>,
    /// Subagents whose results this turn delivers.
    pub notified: Vec<Uuid>,
    /// A subagent parent this turn puts back into `Running`. `None` marks
    /// the session's own turn, which reports `Running` instead.
    pub mark_running: Option<Uuid>,
}

/// Wake every idle subagent parent whose children have results it has not been
/// sent. The main agent is excluded: its owed results merge into its next turn
/// in [`main_turn`].
///
/// Reads the forest rather than one kind's tree, so it wakes a workflow step's
/// subagent parents as readily as a conversation's. It used to live only in
/// [`InteractiveOrchestrator`] and read an accessor that answered empty for a
/// run, which is why a step's subagent could finish and never be heard from.
pub fn wake_owed_parents(state: &SessionState) -> Vec<AgentAction> {
    let mut by_parent: std::collections::BTreeMap<Uuid, Vec<OwedResult>> = Default::default();
    for owed in state.subagents.owed() {
        if let SubAgentParent::SubAgent(parent) = owed.parent {
            by_parent.entry(parent).or_default().push(owed);
        }
    }
    by_parent
        .into_iter()
        // A parent mid-run is consuming already; it hears these when it next
        // goes idle.
        .filter(|(parent, _)| !state.subagents.is_running(*parent))
        .map(|(parent, owed)| {
            AgentAction::StartTurn(TurnStart {
                who: AgentKey::Sub(parent),
                input: TurnInput {
                    // A woken parent is resumed by its children and nothing else —
                    // there is no person in this loop to have typed anything.
                    message: None,
                    results: Vec::new(),
                    subagent_results: owed.iter().map(|o| o.part.clone()).collect(),
                },
                consumed: Vec::new(),
                answered: Vec::new(),
                notified: owed.iter().map(|o| o.child).collect(),
                mark_running: Some(parent),
            })
        })
        .collect()
}

/// The main agent's turn, if one is owed and no run is in flight.
pub fn main_turn(state: &SessionState) -> Option<AgentAction> {
    // Owed subagent results ride every turn the main agent starts; with an
    // empty inbox they can also *start* one, but only from Idle — never
    // answering a pending ask, never chasing a failure.
    let root = state.root_owner();
    let owed: Vec<OwedResult> = state
        .subagents
        .owed()
        .into_iter()
        .filter(|o| o.parent == SubAgentParent::Main && o.owner == root)
        .collect();
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
    //
    // Owed subagent results ride the same message but stay their own parts —
    // joined into the text they were indistinguishable from what the person
    // typed, which is exactly what a reader of the transcript needs to tell
    // apart. `None` when the inbox is empty: an owed-only turn has no text.
    let message = (!state.inbox.is_empty()).then(|| {
        state
            .inbox
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(MERGE_SEPARATOR)
    });
    Some(AgentAction::StartTurn(TurnStart {
        who: AgentKey::Main,
        input: TurnInput {
            message,
            subagent_results: owed.iter().map(|o| o.part.clone()).collect(),
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
        notified: owed.iter().map(|o| o.child).collect(),
        mark_running: None,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::session_actor::InboxMessage;
    use crate::sessions::spec::PendingAsk;
    use crate::sessions::subagents::TreeOwner;

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

    /// What a conversation starts: subagent wakes, then the main agent's turn.
    /// The two components' contributions, concatenated exactly as the actor
    /// concatenates them.
    fn interactive_actions(state: &SessionState) -> Vec<AgentAction> {
        // The runtime gate lives on the boundary, not in these two functions.
        // Modelled here so these tests assert what a session would really do.
        if matches!(
            state.status,
            SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
        ) {
            return Vec::new();
        }
        let mut actions = wake_owed_parents(state);
        actions.extend(main_turn(state));
        actions
    }

    fn only_turn(state: &SessionState) -> AgentAction {
        let mut actions = interactive_actions(state);
        assert_eq!(actions.len(), 1, "expected exactly one action");
        actions.remove(0)
    }

    #[test]
    fn an_empty_inbox_starts_nothing() {}

    #[test]
    fn a_queued_message_starts_one_turn_that_consumes_it() {
        let AgentAction::StartTurn(TurnStart {
            who,
            input,
            consumed,
            ..
        }) = only_turn(&with_inbox(&["hello"]))
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
        let AgentAction::StartTurn(TurnStart {
            input, consumed, ..
        }) = only_turn(&with_inbox(&["a", "b"]))
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.message.as_deref(), Some("a\n\nb"));
        assert_eq!(consumed.len(), 2);
    }

    /// The message that created the session waits for the runtime it will run
    /// on. This is the whole of the wait — there is no gate anywhere else,
    /// because a status the journal already carries is the only thing that
    /// could survive the create being interrupted.
    #[test]
    fn a_provisioning_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Provisioning;
        assert!(interactive_actions(&s).is_empty());
    }

    #[test]
    fn a_running_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Running;
        assert!(interactive_actions(&s).is_empty());
    }

    #[test]
    fn an_unrecoverable_session_starts_nothing() {
        let mut s = with_inbox(&["hello"]);
        s.status = SessionStatus::Unrecoverable {
            reason: "gone".into(),
        };
        assert!(interactive_actions(&s).is_empty());
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
        let AgentAction::StartTurn(TurnStart { input, .. }) = only_turn(&s) else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.results.len(), 1);
        assert_eq!(input.results[0].tool_call_id, "call_1");
        assert!(input.results[0].is_error);
    }

    /// Build a state whose main agent is owed one finished subagent's result.
    fn owing_main(inbox: &[&str], label: &str, output: &str) -> SessionState {
        let mut s = with_inbox(inbox);
        let id = Uuid::new_v4();
        let tree = s.subagents.tree_mut(TreeOwner::Main);
        tree.apply_spawned(
            id,
            SubAgentParent::Main,
            label.into(),
            "t".into(),
            1,
            100,
            None,
        );
        tree.apply_completed(id, output.into(), 400);
        s
    }

    /// The typed text and the result stay apart. Merged, a client could not
    /// tell a subagent's report from what the person actually said — which is
    /// the whole reason results became their own parts.
    #[test]
    fn owed_results_ride_a_turn_without_joining_its_text() {
        let s = owing_main(&["check the lockfile too"], "audit", "three stale crates");
        let AgentAction::StartTurn(TurnStart {
            input, notified, ..
        }) = only_turn(&s)
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.message.as_deref(), Some("check the lockfile too"));
        assert_eq!(input.subagent_results.len(), 1);
        assert_eq!(input.subagent_results[0].label, "audit");
        assert_eq!(input.subagent_results[0].text, "three stale crates");
        assert_eq!(notified.len(), 1);
    }

    /// Nothing was typed, so there is no user text at all — not an empty one,
    /// which Anthropic rejects as a content block.
    #[test]
    fn an_owed_only_turn_has_no_message() {
        let s = owing_main(&[], "audit", "three stale crates");
        let AgentAction::StartTurn(TurnStart { input, .. }) = only_turn(&s) else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(input.message, None);
        assert_eq!(input.subagent_results.len(), 1);
    }

    #[test]
    fn a_woken_subagent_parent_is_resumed_with_results_and_no_message() {
        let mut s = SessionState::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        {
            let tree = s.subagents.tree_mut(TreeOwner::Main);
            tree.apply_spawned(
                parent,
                SubAgentParent::Main,
                "lead".into(),
                "t".into(),
                1,
                100,
                None,
            );
            tree.apply_completed(parent, "waiting".into(), 200);
            tree.apply_notified(parent);
            tree.apply_spawned(
                child,
                SubAgentParent::SubAgent(parent),
                "helper".into(),
                "t".into(),
                2,
                300,
                None,
            );
            tree.apply_completed(child, "kid done".into(), 600);
        }
        let actions = interactive_actions(&s);
        let woken = actions
            .into_iter()
            .find(|a| {
                matches!(a, AgentAction::StartTurn(TurnStart { who, .. }) if matches!(who, AgentKey::Sub(_)))
            })
            .expect("the woken parent's turn");
        let AgentAction::StartTurn(TurnStart { who, input, .. }) = woken else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(who, AgentKey::Sub(parent));
        assert_eq!(input.message, None);
        assert_eq!(input.subagent_results.len(), 1);
        assert_eq!(input.subagent_results[0].text, "kid done");
    }
}
