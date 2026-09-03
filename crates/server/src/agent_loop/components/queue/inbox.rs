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

/// The next thing the queue is offering, and what taking it consumes.
///
/// One offer at a time, in a fixed order of precedence, because each is a
/// different kind of work: taking it is a decision the caller makes, and what
/// it *does* with it is none of this module's business. Whoever takes an offer
/// journals the ids in `consumed`, which is what removes them from the queue.
#[derive(Debug, Clone, PartialEq)]
pub enum Offer {
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
    Input(Box<Turn>),
}

/// Everything an agent is about to be resumed with, and what that consumes.
///
/// Every field is what the actor needs to journal the turn, so nothing below
/// this re-derives a decision made here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// Ids of the queue items this turn carries.
    pub consumed: Vec<String>,
    /// Tool-call ids of the questions this turn *answered*. Empty when the turn
    /// abandoned them instead — the two are deliberately not the same thing.
    pub answered: Vec<String>,
    pub message: Option<String>,
    /// What the people who sent this turn's messages attached, in order.
    ///
    /// Beside `message` rather than inside it because several queued messages
    /// merge into one user turn: the text joins, and so do the attachments.
    pub artifacts: Vec<horsie_models::agent::ArtifactRef>,
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

/// What the queue is offering now, if anything.
///
/// `None` means "nothing to take", and there are exactly two reasons for it:
/// nothing is queued, or the agent is parked on questions and nothing queued
/// is entitled to abandon them. Being *busy* is not one of them — that is the
/// caller's own business, checked before this is ever asked, because work in
/// flight is not a fact about the queue.
///
/// The precedence is about what a loss costs. A sub session waiting on a
/// summary is stuck for ever if its summary is skipped, so it goes first. A
/// `/compact` goes before the message that arrived with it because the point
/// of typing it is to shrink the context the *next* turn reads. Everything
/// else is one merged input.
#[must_use]
pub fn queued_offer(
    inbox: &[Incoming],
    asks: &[crate::agent_loop::AskedQuestion],
) -> Option<Offer> {
    if inbox.is_empty() {
        return None;
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
            | Incoming::Compact { .. } => None,
        })
        .collect();
    if !sub_sessions.is_empty() {
        return Some(Offer::Summary {
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
            | Incoming::SubSession { .. } => None,
        })
        .collect();
    if !compactions.is_empty() {
        return Some(Offer::Compact {
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
            artifacts: Vec::new(),
        })
        .collect();
    Some(Offer::Input(Box::new(turn)))
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
            // An answer the person typed; a form that accepts a file is a
            // separate feature.
            artifacts: Vec::new(),
        })
        .collect();
    Ok(turn)
}

/// Fold the whole queue into one turn's input. Never partial: an agent that is
/// starting a turn at all is starting it on everything it has been told.
fn drain(inbox: &[Incoming]) -> Turn {
    // A `/compact` and a `/summary-n-fork` are instructions to the server, not
    // input, and they are taken as their own offers. Left in the queue here,
    // they would be crossed off by a turn that did nothing about them.
    let inbox: Vec<&Incoming> = inbox
        .iter()
        .filter(|i| !matches!(i, Incoming::Compact { .. } | Incoming::SubSession { .. }))
        .collect();
    let texts: Vec<&str> = inbox.iter().copied().filter_map(Incoming::text).collect();
    Turn {
        consumed: inbox.iter().map(|i| i.id().to_string()).collect(),
        answered: Vec::new(),
        // `None`, not an empty string, when nothing contributed text: Anthropic
        // rejects an empty content block, so a report-only turn must have no
        // user message at all rather than a blank one.
        message: (!texts.is_empty()).then(|| texts.join(MERGE_SEPARATOR)),
        artifacts: inbox
            .iter()
            .copied()
            .filter_map(|i| match i {
                Incoming::User { artifacts, .. } => Some(artifacts.clone()),
                // Nothing else can carry one: a timer and a `Stop` hook are
                // server text, and a subagent reports through its own part.
                Incoming::SubAgent { .. }
                | Incoming::Timer { .. }
                | Incoming::Continue { .. }
                | Incoming::Compact { .. }
                | Incoming::SubSession { .. } => None,
            })
            .flatten()
            .collect(),
        subagent_results: inbox
            .iter()
            .copied()
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
    }
}
