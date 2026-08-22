//! What is addressed to an agent, and when it becomes a turn.
//!
//! An agent has one queue, and everything that arrives for it goes in the same
//! one: a person's message, a subagent's report, a timer firing, a `Stop` hook
//! saying to keep going. They differ in what they contribute to the turn, not
//! in how they are held — which is the whole reason this is one enum rather
//! than four fields.
//!
//! No actors and no I/O, so the decision is unit-testable against a hand-built
//! queue. [`AgentActor`](crate::agent_loop::AgentActor) owns the queue; this
//! owns the rule.

use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};

/// Separator between messages merged into one turn.
///
/// Anthropic requires alternating roles, so several queued messages become one
/// user turn rather than consecutive user ones. Provenance survives in the
/// `Received` events.
pub const MERGE_SEPARATOR: &str = "\n\n";

/// The tool result recorded for a question the user walked away from.
pub const ABANDONED_ASK_RESULT: &str = "not answered — the user sent a new message instead";

/// One accepted-but-undelivered thing addressed to an agent.
///
/// Every variant carries an `id` so a turn can name exactly what it consumed,
/// which is what makes the fold replayable and lets a client cross off the
/// message it is watching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Incoming {
    /// A person typed something.
    User { id: String, text: String },
    /// A subagent this agent spawned finished, and owes it the report.
    SubAgent {
        id: String,
        part: Box<SubAgentResultPart>,
    },
    /// A timer this agent armed fired.
    Timer { id: String, message: String },
    /// A `Stop` hook blocked the end of a turn, so the turn continues with the
    /// hook's reason as its input.
    Continue { id: String, reason: String },
    /// Someone typed `/compact`.
    ///
    /// Queued rather than acted on directly so it happens *in order*: a turn in
    /// flight finishes first, and the compaction is journaled between the same
    /// two messages a reader sees it between. It is not a prompt and never
    /// reaches the model — `instructions` only steers the summariser.
    Compact {
        id: String,
        instructions: Option<String>,
    },
    /// Someone typed `/summary-n-fork`, and `sub_session` is the one waiting
    /// on the summary.
    ///
    /// Queued rather than run out of band so that accepting the command and
    /// this agent becoming busy are the same event: a summary taken while the
    /// session it summarises is still answering describes a history the
    /// branch marker does not. Nothing here is ever said to the model — this
    /// agent produces the summary and keeps its own history exactly as it was.
    SubSession { id: String, sub_session: uuid::Uuid },
}

impl Incoming {
    /// This item's identity, for `consumed`.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::User { id, .. }
            | Self::SubAgent { id, .. }
            | Self::Timer { id, .. }
            | Self::Continue { id, .. }
            | Self::Compact { id, .. }
            | Self::SubSession { id, .. } => id,
        }
    }

    /// Whether this is a person speaking.
    ///
    /// The one distinction the drain rule needs, and it decides a single
    /// thing: whether this item may override a park. A person who types while
    /// the agent is waiting on them has changed their mind — "never mind, do
    /// this instead" — and that is the only thing entitled to abandon the
    /// questions. News that merely *arrived* (a subagent finishing, a timer
    /// firing) has no opinion about the questions and waits its turn.
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// What this item contributes to the turn's user message, if anything.
    #[must_use]
    fn text(&self) -> Option<&str> {
        match self {
            Self::User { text, .. } | Self::Timer { message: text, .. } => Some(text),
            Self::Continue { reason, .. } => Some(reason),
            // A report is its own content part, never merged into the text:
            // joined in, a reader could not tell a subagent's words from the
            // person's, and both would render as one user bubble.
            Self::SubAgent { .. } => None,
            // `/compact` is an instruction to the *server*. Merging it into the
            // turn's text would send the model the word "compact" and compact
            // nothing. `/summary-n-fork` is the same, and its message was never
            // addressed to this agent — it belongs to the sub session.
            Self::Compact { .. } | Self::SubSession { .. } => None,
        }
    }
}

/// A summary a turn was asked for, and what becomes of it.
///
/// One type rather than two turn fields because the two are mutually exclusive
/// by nature: both spend a provider call reading the same history, and running
/// both in one turn would summarise a summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summarise {
    /// `/compact`: fold the summary back into this agent's own history behind a
    /// boundary. The `Option` is the focus instructions — `None` is a bare
    /// `/compact`.
    Compact(Option<String>),
    /// `/summary-n-fork`: the summary is not this session's to keep. It
    /// seeds these sub sessions, and this history is left exactly as it was.
    ///
    /// A list, not one id, because sub sessions queued into the same turn
    /// share a branch point — nothing can append between them — so they are
    /// entitled to the same summary rather than to a provider call each.
    SubSession(Vec<uuid::Uuid>),
}

/// Everything an agent is about to be resumed with, and what that consumes.
///
/// Every field is what the actor needs to journal the turn, so nothing below
/// this re-derives a decision made here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// Ids of the queue items this turn carries.
    pub consumed: Vec<String>,
    /// A summarisation this turn carries, and what becomes of the summary.
    ///
    /// It rides on the turn rather than being acted on at enqueue so it
    /// happens in order — a turn in flight finishes first, and the summary
    /// describes the history a reader sees it taken from.
    pub summarise: Option<Summarise>,
    /// Tool-call ids of the questions this turn *answered*. Empty when the turn
    /// abandoned them instead — the two are deliberately not the same thing.
    pub answered: Vec<String>,
    pub message: Option<String>,
    pub subagent_results: Vec<SubAgentResultPart>,
    pub results: Vec<ToolResultInput>,
}

/// One answer to one pending question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub tool_call_id: String,
    pub text: String,
}

/// Why a set of answers was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerError {
    /// This agent is not parked on anything answerable.
    NothingPending,
    /// The answers did not cover the pending questions exactly.
    Incomplete {
        missing: Vec<String>,
        unexpected: Vec<String>,
    },
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NothingPending => write!(f, "this agent is not waiting on an answer"),
            Self::Incomplete {
                missing,
                unexpected,
            } => write!(
                f,
                "every pending question must be answered together (missing: [{}]; not pending: [{}])",
                missing.join(", "),
                unexpected.join(", ")
            ),
        }
    }
}

/// Whether the queue may start a turn now, and what that turn carries.
///
/// `None` means "not yet", and there are exactly two reasons for it: nothing
/// is queued, or the agent is parked on questions and nothing queued is
/// entitled to abandon them. Being *busy* is not one of them — that is the
/// actor's own business, checked before this is ever asked, because a run in
/// flight is not a fact about the queue.
#[must_use]
pub fn queued_turn(inbox: &[Incoming], asks: &[crate::agent_loop::AskedQuestion]) -> Option<Turn> {
    if inbox.is_empty() {
        return None;
    }
    if !asks.is_empty() && !inbox.iter().any(Incoming::is_user) {
        return None;
    }
    let mut turn = drain(inbox);
    // Abandoned, not answered: every parked call still gets a result, so
    // nothing dangles on the wire, but the result says the question went
    // unanswered. Answering for real goes through `answered_turn`, which
    // requires all of them at once.
    turn.results = asks
        .iter()
        .filter_map(|ask| ask.tool_call_id.clone())
        .map(|tool_call_id| ToolResultInput {
            tool_call_id,
            output: ABANDONED_ASK_RESULT.to_string(),
            is_error: true,
        })
        .collect();
    Some(turn)
}

/// The turn an answered park starts: the answers, plus whatever queued behind
/// them.
///
/// Refused unless the answers cover the pending questions exactly. A
/// half-answered park could not resume anyway — the run would go back to the
/// provider with a `tool_use` that has no result — and refusing costs nothing,
/// because nothing has been journaled yet.
pub fn answered_turn(
    inbox: &[Incoming],
    asks: &[crate::agent_loop::AskedQuestion],
    answers: Vec<AskAnswer>,
) -> Result<Turn, AnswerError> {
    let pending: std::collections::HashSet<String> =
        asks.iter().filter_map(|a| a.tool_call_id.clone()).collect();
    if pending.is_empty() {
        return Err(AnswerError::NothingPending);
    }
    let answered: std::collections::HashSet<String> =
        answers.iter().map(|a| a.tool_call_id.clone()).collect();
    if answered != pending {
        let mut missing: Vec<String> = pending.difference(&answered).cloned().collect();
        let mut unexpected: Vec<String> = answered.difference(&pending).cloned().collect();
        missing.sort();
        unexpected.sort();
        return Err(AnswerError::Incomplete {
            missing,
            unexpected,
        });
    }
    // The queue rides along rather than waiting for another boundary: a
    // subagent that finished while the person was typing their answer is news
    // the same turn wants, and holding it back would strand it until something
    // else happened to start a turn.
    let mut turn = drain(inbox);
    turn.answered = answers.iter().map(|a| a.tool_call_id.clone()).collect();
    turn.results = answers
        .into_iter()
        .map(|a| ToolResultInput {
            tool_call_id: a.tool_call_id,
            output: a.text,
            is_error: false,
        })
        .collect();
    Ok(turn)
}

/// Fold the whole queue into one turn's input. Never partial: an agent that is
/// starting a turn at all is starting it on everything it has been told.
/// The one summary this turn takes, out of everything that asked for one.
///
/// A turn takes at most one: they all read the same history for the same
/// provider call, and running two back to back would summarise a summary.
///
/// Sub sessions win over a queued `/compact`, and every sub session in the
/// drain shares the result. The asymmetry is deliberate and about what a loss
/// costs — a dropped compaction is an optimisation that did not happen, and
/// the automatic check runs again on the very next iteration, while a dropped
/// sub session leaves a session stuck in `Provisioning` with nobody left to
/// finish it.
fn summarise(inbox: &[Incoming]) -> Option<Summarise> {
    let sub_sessions: Vec<uuid::Uuid> = inbox
        .iter()
        .filter_map(|i| match i {
            Incoming::SubSession { sub_session, .. } => Some(*sub_session),
            Incoming::User { .. }
            | Incoming::SubAgent { .. }
            | Incoming::Timer { .. }
            | Incoming::Continue { .. }
            | Incoming::Compact { .. } => None,
        })
        .collect();
    if !sub_sessions.is_empty() {
        return Some(Summarise::SubSession(sub_sessions));
    }
    // The newest `/compact` wins: they ask for the same thing.
    inbox.iter().rev().find_map(|i| match i {
        Incoming::Compact { instructions, .. } => Some(Summarise::Compact(instructions.clone())),
        Incoming::User { .. }
        | Incoming::SubAgent { .. }
        | Incoming::Timer { .. }
        | Incoming::Continue { .. }
        | Incoming::SubSession { .. } => None,
    })
}

fn drain(inbox: &[Incoming]) -> Turn {
    let texts: Vec<&str> = inbox.iter().filter_map(Incoming::text).collect();
    Turn {
        consumed: inbox.iter().map(|i| i.id().to_string()).collect(),
        answered: Vec::new(),
        // `None`, not an empty string, when nothing contributed text: Anthropic
        // rejects an empty content block, so a report-only turn must have no
        // user message at all rather than a blank one.
        message: (!texts.is_empty()).then(|| texts.join(MERGE_SEPARATOR)),
        subagent_results: inbox
            .iter()
            .filter_map(|i| match i {
                Incoming::SubAgent { part, .. } => Some((**part).clone()),
                Incoming::User { .. }
                | Incoming::Timer { .. }
                | Incoming::Continue { .. }
                | Incoming::Compact { .. }
                | Incoming::SubSession { .. } => None,
            })
            .collect(),
        results: Vec::new(),
        summarise: summarise(inbox),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! The queue rule: what merges, what waits, and what a park does and does
    //! not survive.
    use super::*;
    use crate::agent_loop::AskedQuestion;

    fn user(id: &str, text: &str) -> Incoming {
        Incoming::User {
            id: id.to_string(),
            text: text.to_string(),
        }
    }

    fn compact(id: &str, instructions: Option<&str>) -> Incoming {
        Incoming::Compact {
            id: id.to_string(),
            instructions: instructions.map(ToString::to_string),
        }
    }

    /// The word "compact" must never reach the model. Merged into the turn's
    /// text it would compact nothing and read as the user saying it.
    #[test]
    fn a_compact_contributes_no_text_to_the_turn() {
        let turn = drain(&[compact("c1", Some("keep the migration details"))]);
        assert_eq!(turn.message, None);
        assert_eq!(
            turn.summarise,
            Some(Summarise::Compact(Some(
                "keep the migration details".to_string()
            )))
        );
        assert_eq!(turn.consumed, vec!["c1".to_string()]);
    }

    #[test]
    fn a_bare_compact_is_some_none_not_none() {
        let turn = drain(&[compact("c1", None)]);
        assert_eq!(
            turn.summarise,
            Some(Summarise::Compact(None)),
            "`Compact(None)` is a compaction with no focus; `None` is no \
             summarisation at all, and the two decide different things"
        );
    }

    /// A compaction rides *with* the message that followed it rather than
    /// displacing it, so nothing a person typed is dropped to make room.
    #[test]
    fn a_compact_queued_beside_a_message_keeps_both() {
        let turn = drain(&[compact("c1", None), user("u1", "and now do this")]);
        assert_eq!(turn.message.as_deref(), Some("and now do this"));
        assert_eq!(turn.summarise, Some(Summarise::Compact(None)));
        assert_eq!(turn.consumed, vec!["c1".to_string(), "u1".to_string()]);
    }

    /// Two compactions in one turn would summarise a summary, which loses
    /// detail for nothing.
    #[test]
    fn several_queued_compactions_collapse_to_the_newest() {
        let turn = drain(&[
            compact("c1", Some("first ask")),
            compact("c2", Some("second ask")),
        ]);
        assert_eq!(
            turn.summarise,
            Some(Summarise::Compact(Some("second ask".to_string())))
        );
        assert_eq!(
            turn.consumed,
            vec!["c1".to_string(), "c2".to_string()],
            "both are still consumed — neither is owed an answer twice"
        );
    }

    #[test]
    fn a_turn_with_no_compact_says_so() {
        assert_eq!(drain(&[user("u1", "hello")]).summarise, None);
    }

    fn sub_session_item(id: &str, sub_session: uuid::Uuid) -> Incoming {
        Incoming::SubSession {
            id: id.to_string(),
            sub_session,
        }
    }

    /// A `/summary-n-fork` is an instruction to the server, exactly as
    /// `/compact` is. Merged into the turn's text it would read as the person
    /// saying "summary-n-fork" to the model.
    #[test]
    fn a_sub_session_contributes_no_text_to_the_turn() {
        let sub_session = uuid::Uuid::from_bytes([3; 16]);
        let turn = drain(&[sub_session_item("f1", sub_session)]);
        assert_eq!(turn.message, None);
        assert_eq!(
            turn.summarise,
            Some(Summarise::SubSession(vec![sub_session]))
        );
        assert_eq!(turn.consumed, vec!["f1".to_string()]);
    }

    /// Sub sessions queued into the same turn cannot have anything between
    /// them, so they branch from the same history and are entitled to one
    /// provider call rather than one each.
    #[test]
    fn sub_sessions_queued_together_share_one_summary() {
        let (a, b) = (
            uuid::Uuid::from_bytes([1; 16]),
            uuid::Uuid::from_bytes([2; 16]),
        );
        let turn = drain(&[sub_session_item("f1", a), sub_session_item("f2", b)]);
        assert_eq!(turn.summarise, Some(Summarise::SubSession(vec![a, b])));
    }

    /// The asymmetry is about what a loss costs. A dropped compaction is an
    /// optimisation that did not happen and the automatic check runs again
    /// immediately; a dropped sub session leaves a session stuck in
    /// `Provisioning` with nobody left to finish it.
    #[test]
    fn a_sub_session_wins_over_a_compaction_queued_after_it() {
        let sub_session = uuid::Uuid::from_bytes([4; 16]);
        let turn = drain(&[sub_session_item("f1", sub_session), compact("c1", None)]);
        assert_eq!(
            turn.summarise,
            Some(Summarise::SubSession(vec![sub_session]))
        );
        assert_eq!(
            turn.consumed,
            vec!["f1".to_string(), "c1".to_string()],
            "both are still consumed — the queue is drained whole"
        );
    }

    fn report(id: &str, label: &str, text: &str) -> Incoming {
        Incoming::SubAgent {
            id: id.to_string(),
            part: Box::new(SubAgentResultPart {
                subagent_id: id.to_string(),
                label: label.to_string(),
                status: "completed".to_string(),
                text: text.to_string(),
                spawned_at_ms: 0,
                ended_at_ms: 0,
            }),
        }
    }

    fn timer(id: &str, message: &str) -> Incoming {
        Incoming::Timer {
            id: id.to_string(),
            message: message.to_string(),
        }
    }

    fn asking(id: &str, question: &str) -> AskedQuestion {
        AskedQuestion {
            tool_call_id: Some(id.to_string()),
            question: question.to_string(),
        }
    }

    #[test]
    fn an_empty_queue_starts_nothing() {
        assert_eq!(queued_turn(&[], &[]), None);
    }

    #[test]
    fn one_message_becomes_a_turn_that_consumes_it() {
        let turn = queued_turn(&[user("m1", "hello")], &[]).unwrap();
        assert_eq!(turn.message.as_deref(), Some("hello"));
        assert_eq!(turn.consumed, vec!["m1".to_string()]);
    }

    #[test]
    fn several_messages_merge_into_one_turn() {
        let turn = queued_turn(&[user("m1", "a"), user("m2", "b")], &[]).unwrap();
        assert_eq!(turn.message.as_deref(), Some("a\n\nb"));
        assert_eq!(turn.consumed.len(), 2);
    }

    /// Merged into the text, a client could not tell a subagent's report from
    /// what the person actually typed — both would render as a user bubble.
    #[test]
    fn a_report_rides_a_turn_without_joining_its_text() {
        let turn = queued_turn(
            &[
                user("m1", "and the lockfile"),
                report("s1", "audit", "3 stale"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(turn.message.as_deref(), Some("and the lockfile"));
        assert_eq!(turn.subagent_results.len(), 1);
        assert_eq!(turn.subagent_results[0].text, "3 stale");
    }

    /// Nothing was typed, so there is no user message at all — not an empty
    /// one, which Anthropic rejects as a content block.
    #[test]
    fn a_report_only_turn_has_no_message() {
        let turn = queued_turn(&[report("s1", "audit", "3 stale")], &[]).unwrap();
        assert_eq!(turn.message, None);
        assert_eq!(turn.subagent_results.len(), 1);
    }

    /// "Never mind, do this instead." Every parked call still gets a result, so
    /// nothing dangles on the wire.
    #[test]
    fn a_message_overrides_a_park_and_abandons_its_questions() {
        let turn = queued_turn(&[user("m1", "never mind")], &[asking("call-1", "which?")]).unwrap();
        assert_eq!(turn.message.as_deref(), Some("never mind"));
        assert_eq!(turn.results.len(), 1);
        assert_eq!(turn.results[0].tool_call_id, "call-1");
        assert!(turn.results[0].is_error);
        assert!(
            turn.answered.is_empty(),
            "abandoning is not answering — answers come through `answered_turn`"
        );
    }

    /// News that merely arrived has no opinion about the questions. It waits,
    /// and the answer (or a message) is what releases it.
    #[test]
    fn a_report_waits_out_a_park_instead_of_overriding_it() {
        let asks = [asking("call-1", "which?")];
        assert_eq!(queued_turn(&[report("s1", "audit", "done")], &asks), None);
        assert_eq!(queued_turn(&[timer("t1", "check now")], &asks), None);
    }

    /// One user message in the queue releases everything queued with it,
    /// reports included — the whole queue goes in, never part of it.
    #[test]
    fn a_message_releases_the_reports_queued_beside_it() {
        let turn = queued_turn(
            &[report("s1", "audit", "done"), user("m1", "never mind")],
            &[asking("call-1", "which?")],
        )
        .unwrap();
        assert_eq!(turn.consumed, vec!["s1".to_string(), "m1".to_string()]);
        assert_eq!(turn.subagent_results.len(), 1);
    }

    #[test]
    fn a_timer_wake_is_the_turns_text() {
        let turn = queued_turn(&[timer("t1", "re-check the build")], &[]).unwrap();
        assert_eq!(turn.message.as_deref(), Some("re-check the build"));
    }

    #[test]
    fn answering_nothing_pending_is_refused() {
        let err = answered_turn(&[], &[], vec![]).unwrap_err();
        assert_eq!(err, AnswerError::NothingPending);
    }

    /// Resuming on half the answers would send the provider a `tool_use` with
    /// no result, which is the 400 the all-or-nothing rule exists to stop.
    #[test]
    fn a_partial_answer_set_is_refused() {
        let asks = [asking("call-1", "which?"), asking("call-2", "which model?")];
        let err = answered_turn(
            &[],
            &asks,
            vec![AskAnswer {
                tool_call_id: "call-1".into(),
                text: "main".into(),
            }],
        )
        .unwrap_err();
        assert_eq!(
            err,
            AnswerError::Incomplete {
                missing: vec!["call-2".to_string()],
                unexpected: vec![],
            }
        );
    }

    #[test]
    fn an_answer_for_a_call_that_is_not_pending_is_refused() {
        let asks = [asking("call-1", "which?")];
        let err = answered_turn(
            &[],
            &asks,
            vec![
                AskAnswer {
                    tool_call_id: "call-1".into(),
                    text: "main".into(),
                },
                AskAnswer {
                    tool_call_id: "call-9".into(),
                    text: "who asked?".into(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            AnswerError::Incomplete {
                missing: vec![],
                unexpected: vec!["call-9".to_string()],
            }
        );
    }

    /// A report that landed while the person was typing their answer rides the
    /// same turn rather than waiting for another boundary that may never come.
    #[test]
    fn a_complete_answer_set_carries_the_queue_with_it() {
        let asks = [asking("call-1", "which?")];
        let turn = answered_turn(
            &[report("s1", "audit", "3 stale")],
            &asks,
            vec![AskAnswer {
                tool_call_id: "call-1".into(),
                text: "main".into(),
            }],
        )
        .unwrap();
        assert_eq!(turn.answered, vec!["call-1".to_string()]);
        assert_eq!(turn.results.len(), 1);
        assert!(!turn.results[0].is_error);
        assert_eq!(turn.results[0].output, "main");
        assert_eq!(turn.consumed, vec!["s1".to_string()]);
        assert_eq!(turn.subagent_results.len(), 1);
    }
}
