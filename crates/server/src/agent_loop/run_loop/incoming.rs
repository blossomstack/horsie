//! Incoming records and their pure projection into provider input.
//!
//! `Received` records append everything addressed to the agent. `Consumed`
//! and `TurnBegan` records identify what has already been taken. Folding those
//! records yields pending messages and exactly one next input; there is no
//! second queue container and no I/O in this module.
use horsie_agentcore::{AgentInput, Message};
use horsie_models::agent::{SubAgentResultPart, ToolResultInput};
use serde::{Deserialize, Serialize};

/// Separator between messages merged into one turn.
///
/// Anthropic requires alternating roles, so several queued messages become one
/// user turn rather than consecutive user ones. Provenance survives in the
/// `Received` events.
const MERGE_SEPARATOR: &str = "\n\n";

/// The tool result recorded for a question the user walked away from.
const ABANDONED_ASK_RESULT: &str = "not answered — the user sent a new message instead";

/// One accepted-but-undelivered thing addressed to an agent.
///
/// Every variant carries an `id` so a turn can name exactly what it consumed,
/// which is what makes the fold replayable and lets a client cross off the
/// message it is watching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Incoming {
    /// A person typed something, and possibly attached something to it.
    User {
        id: String,
        text: String,
        /// Ids only. The bytes live in the artifact service, and a queue entry
        /// is journaled — inlining them would put base64 in the log.
        #[serde(default)]
        artifacts: Vec<horsie_models::agent::ArtifactRef>,
    },
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
    /// Answers to every question on which the agent is currently parked.
    Answers { id: String, answers: Vec<AskAnswer> },
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
            | Self::Answers { id, .. }
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
            Self::SubAgent { .. } | Self::Answers { .. } => None,
            // `/compact` is an instruction to the *server*. Merging it into the
            // turn's text would send the model the word "compact" and compact
            // nothing. `/summary-n-fork` is the same, and its message was never
            // addressed to this agent — it belongs to the sub session.
            Self::Compact { .. } | Self::SubSession { .. } => None,
        }
    }
}

/// The next history-derived input and the records taking it consumes.
///
/// One offer at a time, in a fixed order of precedence, because each is a
/// different kind of work: taking it is a decision the caller makes, and what
/// it *does* with it is none of this module's business. Whoever takes an offer
/// journals the ids in `consumed`, which is what removes them from the queue.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PendingInput {
    /// `/summary-n-fork`: the summary is not this session's to keep. It seeds
    /// these sub sessions, and this history is left exactly as it was.
    ///
    /// A list, not one id, because sub sessions queued together share a branch
    /// point — nothing can append between them — so they are entitled to the
    /// same summary rather than to a provider call each.
    Summary {
        consumed: Vec<String>,
        sub_sessions: Vec<uuid::Uuid>,
    },
    /// `/compact`: fold the summary back into this agent's own history behind
    /// a boundary. `instructions` is the focus the user typed, if any.
    Compact {
        consumed: Vec<String>,
        instructions: Option<String>,
    },
    /// Everything queued that is *addressed to the model*, merged into one
    /// input.
    Input(Box<TurnInput>),
}

impl PendingInput {
    #[must_use]
    pub fn consumed(&self) -> &[String] {
        match self {
            Self::Summary { consumed, .. } | Self::Compact { consumed, .. } => consumed,
            Self::Input(input) => &input.consumed,
        }
    }
}

/// Everything an agent is about to be resumed with, and what that consumes.
///
/// Every field is what the actor needs to journal the turn, so nothing below
/// this re-derives a decision made here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnInput {
    /// Ids of the queue items this turn carries.
    pub consumed: Vec<String>,
    /// Answers carried by this turn.
    pub answered: Vec<AskAnswer>,
    /// Tool calls abandoned by a new user message rather than answered.
    pub abandoned: Vec<String>,
    pub message: Option<String>,
    /// What the people who sent this turn's messages attached, in order.
    ///
    /// Beside `message` rather than inside it because several queued messages
    /// merge into one user turn: the text joins, and so do the attachments.
    pub artifacts: Vec<horsie_models::agent::ArtifactRef>,
    pub subagent_results: Vec<SubAgentResultPart>,
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
    /// The answer was valid but could not be written durably.
    Unavailable(String),
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
            Self::Unavailable(error) => write!(f, "could not save the answer: {error}"),
        }
    }
}

/// What durable history offers next, if anything.
///
/// `None` means either nothing is pending, or the agent is parked on questions
/// and no pending input may abandon them. Foreground activity is checked by the
/// run loop before this projection is asked.
///
/// The precedence is about what a loss costs. A sub session waiting on a
/// summary is stuck for ever if its summary is skipped, so it goes first. A
/// `/compact` goes before the message that arrived with it because the point
/// of typing it is to shrink the context the *next* turn reads. Everything
/// else is one merged input.
#[must_use]
pub(crate) fn next_input(
    inbox: &[Incoming],
    asks: &[crate::agent_loop::AskedQuestion],
) -> Option<PendingInput> {
    if inbox.is_empty() {
        return None;
    }
    if !asks.is_empty()
        && inbox
            .iter()
            .any(|item| matches!(item, Incoming::Answers { .. }))
    {
        let turn = drain(inbox);
        return (!turn.consumed.is_empty()).then(|| PendingInput::Input(Box::new(turn)));
    }
    // Parked, and nothing queued is a person changing their mind: hold
    // everything, including a `/compact`, until the questions are answered.
    if !asks.is_empty() && !inbox.iter().any(Incoming::is_user) {
        return None;
    }
    let sub_sessions: Vec<(String, uuid::Uuid)> = inbox
        .iter()
        .filter_map(|i| match i {
            Incoming::SubSession { id, sub_session } => Some((id.clone(), *sub_session)),
            Incoming::User { .. }
            | Incoming::SubAgent { .. }
            | Incoming::Timer { .. }
            | Incoming::Continue { .. }
            | Incoming::Answers { .. }
            | Incoming::Compact { .. } => None,
        })
        .collect();
    if !sub_sessions.is_empty() {
        return Some(PendingInput::Summary {
            consumed: sub_sessions.iter().map(|(id, _)| id.clone()).collect(),
            sub_sessions: sub_sessions.into_iter().map(|(_, s)| s).collect(),
        });
    }
    let compactions: Vec<(String, Option<String>)> = inbox
        .iter()
        .filter_map(|i| match i {
            Incoming::Compact { id, instructions } => Some((id.clone(), instructions.clone())),
            Incoming::User { .. }
            | Incoming::SubAgent { .. }
            | Incoming::Timer { .. }
            | Incoming::Continue { .. }
            | Incoming::Answers { .. }
            | Incoming::SubSession { .. } => None,
        })
        .collect();
    if !compactions.is_empty() {
        return Some(PendingInput::Compact {
            consumed: compactions.iter().map(|(id, _)| id.clone()).collect(),
            // The newest wins: they ask for the same thing, and the last
            // instructions typed are the ones the user is thinking of.
            instructions: compactions
                .into_iter()
                .next_back()
                .and_then(|(_, instructions)| instructions),
        });
    }
    let mut turn = drain(inbox);
    if turn.consumed.is_empty() {
        return None;
    }
    // Abandoned, not answered: every parked call still gets a result so
    // nothing dangles on the provider wire.
    turn.abandoned = asks
        .iter()
        .filter_map(|ask| ask.tool_call_id.clone())
        .collect();
    Some(PendingInput::Input(Box::new(turn)))
}

/// Refuse answer sets that do not cover the pending questions exactly.
pub(crate) fn validate_answers(
    asks: &[crate::agent_loop::AskedQuestion],
    answers: &[AskAnswer],
) -> Result<(), AnswerError> {
    let pending: std::collections::HashSet<String> = asks
        .iter()
        .filter_map(|ask| ask.tool_call_id.clone())
        .collect();
    if pending.is_empty() {
        return Err(AnswerError::NothingPending);
    }
    let answered: std::collections::HashSet<String> = answers
        .iter()
        .map(|answer| answer.tool_call_id.clone())
        .collect();
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
    Ok(())
}

/// Fold every model-addressed item into one turn.
pub(crate) fn drain(inbox: &[Incoming]) -> TurnInput {
    let inbox: Vec<&Incoming> = inbox
        .iter()
        .filter(|item| !matches!(item, Incoming::Compact { .. } | Incoming::SubSession { .. }))
        .collect();
    let texts: Vec<&str> = inbox.iter().copied().filter_map(Incoming::text).collect();
    TurnInput {
        consumed: inbox.iter().map(|item| item.id().to_string()).collect(),
        answered: inbox
            .iter()
            .filter_map(|item| match item {
                Incoming::Answers { answers, .. } => Some(answers.clone()),
                Incoming::User { .. }
                | Incoming::SubAgent { .. }
                | Incoming::Timer { .. }
                | Incoming::Continue { .. }
                | Incoming::Compact { .. }
                | Incoming::SubSession { .. } => None,
            })
            .flatten()
            .collect(),
        abandoned: Vec::new(),
        message: (!texts.is_empty()).then(|| texts.join(MERGE_SEPARATOR)),
        artifacts: inbox
            .iter()
            .filter_map(|item| match item {
                Incoming::User { artifacts, .. } => Some(artifacts.clone()),
                Incoming::SubAgent { .. }
                | Incoming::Timer { .. }
                | Incoming::Continue { .. }
                | Incoming::Answers { .. }
                | Incoming::Compact { .. }
                | Incoming::SubSession { .. } => None,
            })
            .flatten()
            .collect(),
        subagent_results: inbox
            .iter()
            .filter_map(|item| match item {
                Incoming::SubAgent { part, .. } => Some((**part).clone()),
                Incoming::User { .. }
                | Incoming::Timer { .. }
                | Incoming::Continue { .. }
                | Incoming::Answers { .. }
                | Incoming::Compact { .. }
                | Incoming::SubSession { .. } => None,
            })
            .collect(),
    }
}

/// Build the exact model-visible messages for one consumed input batch.
pub(crate) fn messages(
    turn: &TurnInput,
    rewritten: Option<&str>,
    message_id: String,
    at_ms: u64,
) -> Vec<Message> {
    let results: Vec<ToolResultInput> = turn
        .answered
        .iter()
        .map(|answer| ToolResultInput {
            tool_call_id: answer.tool_call_id.clone(),
            output: answer.text.clone(),
            is_error: false,
            artifacts: Vec::new(),
        })
        .chain(turn.abandoned.iter().map(|tool_call_id| ToolResultInput {
            tool_call_id: tool_call_id.clone(),
            output: ABANDONED_ASK_RESULT.to_string(),
            is_error: true,
            artifacts: Vec::new(),
        }))
        .collect();
    let text = rewritten
        .map(str::to_string)
        .or_else(|| turn.message.clone());
    let starts_user_turn = text.is_some() || !turn.subagent_results.is_empty();
    let mut messages = Vec::new();
    if starts_user_turn {
        if !results.is_empty() {
            messages.push(AgentInput::tool_results(results).to_message(at_ms));
        }
        messages.push(
            AgentInput::user_message_with_results(
                message_id,
                text.unwrap_or_default(),
                turn.subagent_results.clone(),
                turn.artifacts.clone(),
            )
            .to_message(at_ms),
        );
    } else if !results.is_empty() {
        messages.push(AgentInput::tool_results(results).to_message(at_ms));
    }
    messages
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn question(id: &str) -> crate::agent_loop::AskedQuestion {
        crate::agent_loop::AskedQuestion {
            tool_call_id: Some(id.to_string()),
            question: "question".to_string(),
            choices: Vec::new(),
            multiple: false,
        }
    }

    #[test]
    fn answers_are_durable_incoming_input() {
        let answers = vec![AskAnswer {
            tool_call_id: "call".into(),
            text: "answer".into(),
        }];
        let inbox = vec![Incoming::Answers {
            id: "answers".into(),
            answers: answers.clone(),
        }];

        let Some(PendingInput::Input(turn)) = next_input(&inbox, &[question("call")]) else {
            panic!("expected provider input");
        };
        assert_eq!(turn.answered, answers);
        assert_eq!(turn.consumed, ["answers"]);
        assert_eq!(messages(&turn, None, "turn".into(), 1).len(), 1);
    }

    #[test]
    fn a_new_message_abandons_every_pending_question() {
        let inbox = vec![Incoming::User {
            id: "user".into(),
            text: "new work".into(),
            artifacts: Vec::new(),
        }];

        let Some(PendingInput::Input(turn)) =
            next_input(&inbox, &[question("one"), question("two")])
        else {
            panic!("expected provider input");
        };
        assert_eq!(turn.abandoned, ["one", "two"]);
        let projected = messages(&turn, None, "turn".into(), 1);
        assert_eq!(projected.len(), 2);
    }

    #[test]
    fn an_answer_must_cover_the_question_set_exactly() {
        let error = validate_answers(
            &[question("one"), question("two")],
            &[AskAnswer {
                tool_call_id: "one".into(),
                text: "answer".into(),
            }],
        )
        .expect_err("partial answers must be refused");
        assert!(matches!(error, AnswerError::Incomplete { .. }));
    }
}
