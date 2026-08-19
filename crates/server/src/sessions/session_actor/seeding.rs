//! Seeding a fork: giving a new conversation the history it branched from.
//!
//! The session's half of `/fork`. The conversation runner decides *that* a
//! branch point is wanted — it asks for [`Action::Seed`] at every boundary
//! until one lands — and everything here is the performing: reading the
//! source's log, taking a summary of it, and handing the result to the fork's
//! own agent. None of it may happen on the mailbox, because a copy reads a
//! whole conversation and a summary is a provider call, so every path below
//! ends in a task that reports back through [`CoreCommand::SeedSettled`].
//!
//! # Two modes, one hand-over
//!
//! `/fork` copies the source's log, scrubbed and cut at the branch point.
//! `/summary-n-fork` copies nothing: the source runs a summarising turn and the
//! fork starts from that alone, which is the entire point of asking for one.
//! Both end at [`hand_over`], which appends one synthetic message saying where
//! the conversation came from — so a fork always knows it is a fork, whether or
//! not it carries the history.
//!
//! # Why the summary is a turn on the source
//!
//! The summary is **queued as an ordinary message on the source's own inbox**,
//! not taken out of band. Out of band the source stayed `Idle` and went on
//! answering, so a reply sent while the summariser ran landed after the branch
//! marker and inside the summary — the marker and the summary described
//! different conversations. Queued, nothing can append between the two.
//!
//! That is also why the answer comes back the long way. The source's turn
//! reports its summary as [`AgentOutcome::ForkSummary`], which is not a turn
//! *ending* at all — the source may still be working — so the session answers
//! it separately and only then hands the fork what it was waiting for.
//!
//! # Nothing records that a seed is in flight
//!
//! Deliberately, and it is what makes a fork abandoned mid-seed repairable. A
//! journaled "seeding" flag would need an event for the process carrying it
//! dying, and there is none — the flag would survive the crash and the fork
//! would wait for a task that no longer exists. Held in memory
//! ([`SessionActor::seeding`]), a reload starts with nothing in flight, the
//! first boundary asks again, and the same seed is built a second time. The
//! fork's agent recognises a history it already has and says so rather than
//! failing, so the repeat is free.

use super::{CommandEffect, CoreCommand, SessionActor, SessionCommand, SessionEvent, SessionState};
use crate::agent_loop::{AgentCommand, AgentState, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::runners::action::{Branch, ForkMode};
use crate::sessions::runners::ids::{AgentId, RunnerId};
use crate::sessions::runners::message::ChildMsg;
use crate::sessions::runners::{RunnerEvent, RunnerState, conversation};
use horsie_actor::{ActorContext, ActorRef};
use horsie_agentcore::{ContentPart, Message, Role, TextPart};
use horsie_models::now_ms;
use uuid::Uuid;

impl SessionActor {
    /// Build one fork's branch point, off the mailbox.
    ///
    /// Nothing is journaled here. What is durable already — the fork's runner,
    /// its branch point and the message it was created with — is everything a
    /// second attempt needs, so this records only that an attempt is in flight
    /// and forgets it when the attempt settles.
    ///
    /// Not reaching the source is a skip rather than a failure: the next
    /// boundary asks again, and a fork that failed can never be retried.
    pub(super) async fn seed_fork(
        &mut self,
        runner: RunnerId,
        fork: AgentId,
        branch: Branch,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) {
        // The dedupe. `Runner::actions` is idempotent and asks at every
        // boundary, so without this a fork would be seeded once per boundary
        // for as long as the first attempt took.
        if !self.seeding.insert(runner) {
            return;
        }
        let Some(source) = self.reach(branch.source, state, ctx) else {
            tracing::warn!(session = %self.id, %runner, "no conversation to seed a fork from");
            self.seeding.remove(&runner);
            return;
        };
        match branch.mode {
            // Everything a copy needs is on the source's mailbox, so it goes
            // straight to the hand-over.
            ForkMode::Copy => {
                let Some(target) = self.reach(fork, state, ctx) else {
                    tracing::warn!(session = %self.id, %runner, "no agent to seed a fork into");
                    self.seeding.remove(&runner);
                    return;
                };
                let title = self.source_title(state, branch.source);
                let me = self.me(ctx);
                let at_seq = branch.source_seq;
                tokio::spawn(async move {
                    let result = copy_into_fork(&source, &target, at_seq, &title).await;
                    let _ = me
                        .tell(SessionCommand::Core(CoreCommand::SeedSettled {
                            runner,
                            result,
                        }))
                        .await;
                });
            }
            // A turn on the source, queued like anything else a person asks of
            // it. The answer arrives at `on_fork_summary`, not here.
            ForkMode::Summary => {
                tokio::spawn(async move {
                    let _ = source
                        .tell(AgentCommand::Enqueue {
                            // Derived from the fork's runner rather than
                            // generated: a second attempt asks for the same
                            // thing, and the id is what says so.
                            item: Incoming::Fork {
                                id: format!("fork-summarise:{runner}"),
                                fork: runner.as_uuid(),
                            },
                            ack: None,
                        })
                        .await;
                });
            }
        }
    }

    /// A `/summary-n-fork` summary, back from the turn that produced it.
    ///
    /// One turn can carry several forks — everything queued together branches
    /// from the same history and is entitled to one provider call — so this
    /// answers each of them from the one result.
    ///
    /// A fork nobody is holding a seed for is dropped rather than reported: a
    /// fork deleted while its summary was being taken is not a failure, it is
    /// the person having changed their mind.
    pub(super) async fn on_fork_summary(
        &mut self,
        state: &SessionState,
        forks: Vec<Uuid>,
        result: Result<String, String>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let mut events = Vec::new();
        for runner in forks.into_iter().map(RunnerId) {
            if !self.seeding.contains(&runner) {
                continue;
            }
            let summary = match &result {
                Ok(summary) => summary.clone(),
                // Recorded on the fork, not delivered to the source: a fork
                // owes nobody a result, so a seed that never landed is that
                // fork's own status.
                Err(error) => {
                    events.extend(
                        self.seed_settled(runner, Err(error.clone()), state, ctx)
                            .await,
                    );
                    continue;
                }
            };
            let Some((fork, source)) = self.fork_and_source(runner, state) else {
                events.extend(
                    self.seed_settled(
                        runner,
                        Err("the fork this summary was for is gone".to_string()),
                        state,
                        ctx,
                    )
                    .await,
                );
                continue;
            };
            let Some(target) = self.reach(fork, state, ctx) else {
                tracing::warn!(session = %self.id, %runner, "no agent to seed a summary fork into");
                // Left in flight: the next boundary asks again, and a summary
                // that cannot be handed over is not a summary that failed.
                self.seeding.remove(&runner);
                continue;
            };
            let title = self.source_title(state, source);
            let me = self.me(ctx);
            tokio::spawn(async move {
                let result = hand_over(&target, AgentState::default(), &summary, &title).await;
                let _ = me
                    .tell(SessionCommand::Core(CoreCommand::SeedSettled {
                        runner,
                        result,
                    }))
                    .await;
            });
        }
        self.persist_and_advance(state, events, ctx).await
    }

    /// One fork's seed settled, one way or the other.
    ///
    /// Two records, because two parties hold one fact. The event is the
    /// conversation runner's own, so the fold that releases the fork and the
    /// fold that fails it are the ones every conversation already has; and the
    /// agent that typed `/fork` is told separately, because *it* has been
    /// carrying this branch as in-flight since the session created it and
    /// nothing else would ever clear it.
    ///
    /// Nothing is delivered either way. A fork owes nobody a result, so a seed
    /// that never landed is that fork's own status rather than a report its
    /// source is waiting on.
    pub(super) async fn seed_settled(
        &mut self,
        runner: RunnerId,
        result: Result<(), String>,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Vec<SessionEvent> {
        self.seeding.remove(&runner);
        // A fork a person deleted while its seed was in flight. Nothing to
        // record, and nowhere to record it.
        let Some(record) = state.record(runner) else {
            return Vec::new();
        };
        let parent = record.parent;
        let (event, moved) = match result {
            Ok(()) => (
                conversation::Event::Seeded,
                ChildMsg::Ready { child: runner },
            ),
            Err(error) => {
                tracing::warn!(session = %self.id, %runner, error, "seeding a fork failed");
                (
                    conversation::Event::SeedFailed {
                        error: error.clone(),
                    },
                    ChildMsg::Failed {
                        child: runner,
                        error,
                    },
                )
            }
        };
        if let Some(source) = parent.and_then(|p| self.reach(p, state, ctx)) {
            let _ = source.tell(AgentCommand::ChildMoved { msg: moved }).await;
        }
        vec![SessionEvent::Runner {
            id: runner,
            event: Box::new(RunnerEvent::Conversation(event)),
            at_ms: now_ms(),
        }]
    }

    /// Where the source's log stands, as the branch point for a fork being
    /// created.
    ///
    /// Read *here*, on the way to journaling the fork, rather than when the
    /// seed is built: journaling the fork writes a `Forked` entry onto the
    /// source's own log, so a number taken any later would hand the fork its
    /// own creation marker. The capability that asked cannot read it — it
    /// carries a zero and says so — because a capability decides and the
    /// session performs.
    ///
    /// `None` when the source cannot be reached, which fails the create rather
    /// than cutting the branch at a guess.
    pub(super) async fn source_log_head(
        &mut self,
        source: AgentId,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
    ) -> Option<u64> {
        self.reach(source, state, ctx)?
            .ask(|reply| AgentCommand::LogHead { reply })
            .await
            .ok()
    }

    /// The fork's agent and the agent it branched from, off the fork's own
    /// record.
    fn fork_and_source(
        &self,
        runner: RunnerId,
        state: &SessionState,
    ) -> Option<(AgentId, AgentId)> {
        let record = state.record(runner)?;
        let RunnerState::Conversation(fork) = &record.state else {
            return None;
        };
        Some((fork.agent, fork.seed.as_ref()?.source))
    }

    /// What to call the conversation a fork came from, in the fork's own seed.
    ///
    /// The session's name for its root, a fork's own for a fork of a fork.
    /// Anything unnamed falls back to a phrase rather than to an id, which
    /// means nothing to a reader.
    fn source_title(&self, state: &SessionState, source: AgentId) -> String {
        let named = state.runner_of(source).and_then(|runner| {
            match state.record(runner).map(|r| &r.state) {
                // The session *is* its root conversation, so the name a person
                // sees in the list is what that conversation is called.
                Some(RunnerState::Conversation(c)) if state.root == runner => {
                    self.spec().name.clone().or_else(|| c.title.clone())
                }
                Some(RunnerState::Conversation(c)) => c.title.clone(),
                Some(RunnerState::SubAgent(_) | RunnerState::Workflow(_))
                | Some(RunnerState::Runtime(_))
                | None => None,
            }
        });
        named.unwrap_or_else(|| "the conversation before this one".to_string())
    }
}

/// Read the source's history at the branch point and hand it to the fork.
async fn copy_into_fork(
    source: &ActorRef<AgentCommand>,
    fork: &ActorRef<AgentCommand>,
    at_seq: u64,
    source_title: &str,
) -> Result<(), String> {
    let state = source
        .ask(|reply| AgentCommand::ForkSeed { at_seq, reply })
        .await
        .map_err(|e| format!("read the conversation to fork: {e}"))?;
    // A copy carries no summary: the history *is* what it was given.
    hand_over(fork, *state, "", source_title).await
}

/// Adopt `state` as the fork's whole history and append the message that frames
/// it.
///
/// The one place both modes end, so "a fork is told where it came from" is a
/// property of this function rather than something each mode has to remember.
async fn hand_over(
    fork: &ActorRef<AgentCommand>,
    state: AgentState,
    summary: &str,
    source_title: &str,
) -> Result<(), String> {
    let seed = Message {
        // The `fork:` prefix the device compaction already uses for
        // `compaction:{n}`, so `prompt_messages` needs no change and a client
        // special-cases an id prefix it special-cases already.
        id: format!("fork:{}", Uuid::new_v4()),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: seed_text(source_title, summary),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    };
    fork.ask(|reply| AgentCommand::SeedFrom {
        state: Box::new(state),
        seed: Box::new(seed),
        reply,
    })
    .await
    .map_err(|e| format!("seed the fork: {e}"))?
}

/// What a fork reads first.
///
/// The title instruction rides here rather than in the system prompt: a prompt
/// section is re-sent every turn and would go on nagging long after the fork was
/// named.
fn seed_text(source_title: &str, summary: &str) -> String {
    let mut out = format!(
        "This conversation was forked from \"{source_title}\". The message that \
         follows sets a new direction — call set_session_title once it is clear."
    );
    if !summary.is_empty() {
        out.push_str("\n\n# Summary of the conversation this was forked from\n\n");
        out.push_str(summary);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{
        BlockingProvider, EchoProvider, FailOnNeedleProvider, actor_fixture, actor_spec_fixture,
        agent_history, send, spawn_session_with_provider, spawn_sub, turn_outcomes, turns_begun,
        wait_for_state,
    };
    use super::*;
    use crate::sessions::addressing::SessionRef;
    use std::sync::Arc;

    /// Type `text` at `agent_id` and hand back the fork it created.
    async fn fork_via(
        session: &SessionRef,
        agent_id: Option<String>,
        text: &str,
    ) -> Result<String, crate::sessions::UserMessageError> {
        session
            .ask(|reply| SessionCommand::UserMessage {
                agent_id,
                text: text.into(),
                reply,
            })
            .await
            .unwrap()
            .map(|a| a.forked_agent.expect("a fork command answers with a fork"))
    }

    /// Every text one agent's log holds, joined — enough to ask whether the
    /// conversation came across.
    async fn transcript(session: &SessionRef, agent_id: Option<String>) -> String {
        agent_history(session, agent_id)
            .await
            .entries
            .iter()
            .map(|e| format!("{:?}", e.body))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether that fork's branch point has landed, read off the session's own
    /// folded state.
    fn is_seeded(state: &SessionState, fork: &str) -> bool {
        conversation_of(state, fork).is_some_and(|c| c.seeded)
    }

    /// One fork's slice, found by the agent id a client holds.
    fn conversation_of<'a>(
        state: &'a SessionState,
        fork: &str,
    ) -> Option<&'a crate::sessions::runners::conversation::State> {
        let agent = AgentId(fork.parse::<Uuid>().ok()?);
        match &state.record(state.runner_of(agent)?)?.state {
            RunnerState::Conversation(c) => Some(c),
            RunnerState::SubAgent(_) | RunnerState::Workflow(_) | RunnerState::Runtime(_) => None,
        }
    }

    /// How many turns began in one agent's log, and how the ones that ended
    /// ended. A page folds exactly this pair — an unmatched `TurnBegan` is what
    /// reads `RUNNING` for ever.
    async fn turn_boundaries(
        session: &SessionRef,
        agent_id: Option<String>,
    ) -> (usize, Vec<horsie_agentcore::TurnOutcome>) {
        let page = agent_history(session, agent_id).await;
        (turns_begun(&page), turn_outcomes(&page))
    }

    /// Wait until `agent_id`'s log holds `turns` turns, all closed, and hand
    /// back how the last one ended.
    ///
    /// An exact count rather than "at least one has ended": the seeded copy
    /// arrives already closed, so a floor would pass on a fork whose own turn
    /// never ends — which is the whole thing under test.
    async fn wait_for_turn_end(
        session: &SessionRef,
        agent_id: Option<String>,
        turns: usize,
    ) -> horsie_agentcore::TurnOutcome {
        let mut last = (0, Vec::new());
        for _ in 0..300 {
            last = turn_boundaries(session, agent_id.clone()).await;
            if last.0 == turns && last.1.len() == turns {
                return last.1.pop().expect("a closed turn has an outcome");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "wanted {turns} turns, all closed; {} began and {} ended: {}",
            last.0,
            last.1.len(),
            transcript(session, agent_id).await
        )
    }

    /// How many turns the session's root conversation has begun.
    async fn main_turns_begun(session: &SessionRef) -> usize {
        turns_begun(&agent_history(session, None).await)
    }

    /// The whole of `/fork`: the fork exists, carries what was said before it,
    /// and answers the message that created it.
    #[tokio::test]
    async fn a_fork_carries_the_conversation_and_answers_its_own_message() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // Closed before forking: a fork taken between the source's answer and
        // its `TurnEnded` seeds an unmatched `TurnBegan`, which is a real
        // hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");

        // Seeded, not merely created: the branch point landing is what releases
        // the message the fork was created with.
        wait_for_state(&journal, id, "the fork is seeded", |s| is_seeded(s, &fork)).await;

        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            forked.contains("the original question"),
            "a copy fork carries the conversation it came from: {forked}"
        );
        assert!(
            forked.contains("forked from"),
            "and is told where it came from: {forked}"
        );
        assert!(
            forked.contains("try the other migration"),
            "and holds the message that created it: {forked}"
        );
    }

    /// Wait until any turn in `agent_id`'s log has ended, and hand back how.
    ///
    /// For a fixture where the copied history carries no boundary of its own —
    /// a source held mid-turn — so the first end to appear is the fork's.
    async fn wait_for_any_turn_end(
        session: &SessionRef,
        agent_id: Option<String>,
    ) -> horsie_agentcore::TurnOutcome {
        for _ in 0..300 {
            if let Some(outcome) =
                turn_outcomes(&agent_history(session, agent_id.clone()).await).pop()
            {
                return outcome;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "no turn ever ended in that log: {}",
            transcript(session, agent_id).await
        )
    }

    /// A summary fork starts small. That is the entire reason to ask for one,
    /// so the source's messages must *not* be in its log.
    #[tokio::test]
    async fn a_summary_fork_does_not_carry_the_source_messages() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "a very long conversation about migrations").await;

        let fork = fork_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the summary fork is seeded", |s| {
            is_seeded(s, &fork)
        })
        .await;

        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            !forked.contains("a very long conversation about migrations"),
            "a summary fork discards the history it summarised: {forked}"
        );
        assert!(
            forked.contains("forked from"),
            "but is still told where it came from: {forked}"
        );
    }

    /// The summary is the source's **own turn**, not a detached read of it.
    ///
    /// This is the whole point of the design. Run out of band, the summariser
    /// left the source `Idle` and answering, so a reply sent while it ran landed
    /// after the branch marker in the source's transcript and inside the fork's
    /// summary — the two described different conversations. Queued, the source
    /// cannot append while the summary is taken, and the proof that it is a turn
    /// is that the source's own log carries one.
    #[tokio::test]
    async fn summarising_for_a_fork_is_a_turn_on_the_conversation_it_branches() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;
        let before = main_turns_begun(&session).await;

        let fork = fork_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the summary fork is seeded", |s| {
            is_seeded(s, &fork)
        })
        .await;

        assert!(
            main_turns_begun(&session).await > before,
            "the source ran a turn to produce the summary; its log holds only \
             {before} turn(s), which is what an out-of-band summariser leaves \
             behind:\n{}",
            transcript(&session, None).await
        );
    }

    /// The branch point is visible where it happened, so scrolling the source
    /// shows where each fork left.
    #[tokio::test]
    async fn the_source_transcript_records_where_a_fork_left() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "first").await;
        let fork = fork_via(&session, None, "/fork branch here")
            .await
            .expect("a fork");

        for _ in 0..200 {
            if transcript(&session, None).await.contains(&fork) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the source never recorded the branch: {}",
            transcript(&session, None).await
        );
    }

    /// A fork's page folds its own log: `TurnBegan` reads `Running` and only a
    /// `TurnEnded` clears it. Without one the page says `RUNNING` for ever —
    /// through reloads *and* restarts, because the status is derived from the
    /// journal rather than from anything live.
    #[tokio::test]
    async fn a_forks_turn_ends_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // The source's turn has to be *closed* before forking, because this
        // test's premise is that the copy carries a closed turn over. A fork
        // taken between the source's answer and its `TurnEnded` seeds an
        // unmatched `TurnBegan` — a real hazard, and not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the fork is seeded", |s| is_seeded(s, &fork)).await;

        // Two: the source's one turn, which the copy carried over already
        // closed, and the fork's own answer to the message that created it.
        assert!(
            matches!(
                wait_for_turn_end(&session, Some(fork.clone()), 2).await,
                horsie_agentcore::TurnOutcome::Ended(_)
            ),
            "a fork's turn ends like any other conversation's: {}",
            transcript(&session, Some(fork)).await
        );
    }

    /// A fork working is not the session working. The two statuses are read off
    /// different runners, and a client shows them side by side, so a fork's turn
    /// must move exactly one of them.
    #[tokio::test]
    async fn a_forks_turn_moves_the_forks_status_and_not_the_sessions() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        wait_for_turn_end(&session, Some(fork.clone()), 2).await;

        let state = wait_for_state(&journal, id, "the fork settles", |s| {
            conversation_of(s, &fork).is_some_and(|c| {
                c.turn == crate::sessions::runners::conversation::TurnStatus::Idle
                    && c.last_activity_ms > 0
            })
        })
        .await;
        assert_eq!(
            crate::sessions::runners::reads::session_status(&state),
            crate::sessions::spec::SessionStatus::Idle,
            "the session's own status belongs to its root conversation"
        );
    }

    /// The reason a fork's turn failed has one place a reader will look for it:
    /// the fork's own page. It used to be dropped with a warning, so a fork
    /// whose turn broke went on reading `RUNNING` and said nothing about why.
    #[tokio::test]
    async fn a_forks_failed_turn_says_so_in_its_own_log() {
        let provider = FailOnNeedleProvider {
            needle: "the doomed branch".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork the doomed branch")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the fork is seeded", |s| is_seeded(s, &fork)).await;

        let outcome = wait_for_turn_end(&session, Some(fork.clone()), 2).await;
        let horsie_agentcore::TurnOutcome::Failed(failed) = &outcome else {
            panic!(
                "a fork's failed turn ends as failed, not {outcome:?}: {}",
                transcript(&session, Some(fork)).await
            );
        };
        assert!(failed.error.contains("bad key"), "{:?}", failed.error);
    }

    /// Stop, addressed to a fork.
    ///
    /// It used to be addressed to nothing: the gate read the *session's* status,
    /// which a fork never moves, so pressing Stop on a fork's page returned
    /// `200` having done nothing at all. The fork went on working, and there was
    /// no way to interrupt it.
    #[tokio::test]
    async fn stopping_a_fork_cancels_that_forks_turn() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_session_with_provider(provider.clone() as Arc<dyn horsie_agentcore::LlmProvider>)
                .await;
        // The source's turn is held open too, so nothing about this test can
        // pass by stopping the root conversation instead.
        send(&session, "the original question").await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the fork is working", |s| {
            conversation_of(s, &fork).is_some_and(|c| {
                c.turn == crate::sessions::runners::conversation::TurnStatus::Running
            })
        })
        .await;

        session
            .ask(|reply| SessionCommand::Stop {
                agent_id: fork.clone(),
                reply,
            })
            .await
            .unwrap()
            .expect("a working fork is stoppable");

        // Any end in this log is the fork's own: the source is deliberately
        // held mid-turn, so the history the copy carried has an *open* turn in
        // it and no boundary of its own.
        let outcome = wait_for_any_turn_end(&session, Some(fork.clone())).await;
        assert!(
            matches!(outcome, horsie_agentcore::TurnOutcome::Stopped(_)),
            "the fork's turn ends as stopped, not {outcome:?}: {}",
            transcript(&session, Some(fork)).await
        );
        let state = wait_for_state(&journal, id, "the source is still working", |_| true).await;
        assert_eq!(
            crate::sessions::runners::reads::session_status(&state),
            crate::sessions::spec::SessionStatus::Running,
            "the source's own turn is untouched — it was not what was stopped"
        );
        provider.release();
    }

    /// **A fork left mid-seed is finished when the session loads.**
    ///
    /// Seeding is the session's own work and has no journal of its own, so
    /// nothing else can complete one a dead process abandoned. What repairs it
    /// is that no event records a seed in flight: the fork's runner still says
    /// `seeded: false`, the boundary a load makes asks for the branch point
    /// exactly as the first boundary did, and the summary is taken again.
    ///
    /// The first summarising call never answers, which is the process dying
    /// mid-seed; the second one does.
    #[tokio::test]
    async fn a_fork_left_mid_seed_is_reseeded_at_load() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "i1", "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(Arc::new(StallsFirstSummary::new())),
        );
        let journal = f.journal();
        let session = f.start(id, actor_spec_fixture()).await;

        // A history to summarise: an empty one summarises to nothing without a
        // provider call at all, so there would be nothing to stall.
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/summary-n-fork carry on elsewhere")
            .await
            .expect("a fork");
        wait_for_state(&journal, id, "the fork is created but not seeded", |s| {
            conversation_of(s, &fork).is_some_and(|c| !c.seeded)
        })
        .await;

        // The process that was carrying the seed is gone, and its summary with
        // it. Nothing durable says the seed was ever started.
        drop(session);
        f.node.restart().await;
        let session = f.start(id, actor_spec_fixture()).await;

        wait_for_state(&journal, id, "the fork is seeded after the reload", |s| {
            is_seeded(s, &fork)
        })
        .await;
        let forked = transcript(&session, Some(fork)).await;
        assert!(
            forked.contains("forked from"),
            "the re-seeded fork still frames where it came from: {forked}"
        );
    }

    /// A subagent's conversation is delegated work, not a branch to take.
    #[tokio::test]
    async fn only_a_conversation_can_be_forked() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;

        let err = fork_via(&session, Some(sub.to_string()), "/fork off you go")
            .await
            .expect_err("a subagent cannot be forked");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("only a conversation")),
            "{err:?}"
        );
    }

    /// A fork with nothing to do is a fork nobody comes back to.
    #[tokio::test]
    async fn a_fork_needs_a_message() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let err = fork_via(&session, None, "/fork")
            .await
            .expect_err("a bare fork is refused");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("needs a message")),
            "{err:?}"
        );
    }

    /// Forks nest: a fork of a fork branches from that fork's log, not the
    /// root's.
    #[tokio::test]
    async fn a_fork_of_a_fork_records_the_fork_it_came_from() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "start").await;
        wait_for_turn_end(&session, None, 1).await;

        let first = fork_via(&session, None, "/fork one").await.expect("a fork");
        wait_for_state(&journal, id, "the first fork is seeded", |s| {
            is_seeded(s, &first)
        })
        .await;

        let second = fork_via(&session, Some(first.clone()), "/fork two")
            .await
            .expect("a fork of a fork");
        let state = wait_for_state(&journal, id, "the second fork exists", |s| {
            conversation_of(s, &second).is_some()
        })
        .await;
        let branch = conversation_of(&state, &second)
            .and_then(|c| c.seed.clone())
            .expect("a fork has a branch point");
        assert_eq!(
            branch.source.to_string(),
            first,
            "a fork of a fork is cut from that fork's log"
        );
    }

    /// The copy stops at the branch point, and the branch point is read before
    /// anything is written.
    ///
    /// Two things ride on that. A message the source has accepted but not yet
    /// answered belongs to the *source* — copied into the fork, both
    /// conversations answer it — and the `Forked` entry recording this very
    /// fork is written onto the source after the branch, so a copy taken at the
    /// log's end would hand the fork a marker pointing at itself.
    #[tokio::test]
    async fn a_fork_does_not_take_over_a_message_queued_on_the_source() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(provider.clone()).await;

        // Hold the source inside a turn, so the next message queues rather
        // than draining.
        send(&session, "the turn that is running").await;
        wait_for_state(&journal, id, "the source is running", |s| {
            crate::sessions::runners::reads::session_status(s)
                == crate::sessions::spec::SessionStatus::Running
        })
        .await;
        send(&session, "QUEUED-FOR-THE-SOURCE").await;

        let fork = fork_via(&session, None, "/fork the fork's own instruction")
            .await
            .expect("a busy conversation can still be forked");
        wait_for_state(&journal, id, "the fork is seeded", |s| is_seeded(s, &fork)).await;

        let forked = transcript(&session, Some(fork)).await;
        // The copied history *records* that the message was queued — that
        // happened before the branch, and the copy says so — but it must never
        // become a message the fork sends: the source queued it because a turn
        // was in flight, and that turn's boundary is what answers it. Answered
        // here too, the person gets two replies to one message.
        assert!(
            !forked.contains(r#"TextPart { text: "QUEUED-FOR-THE-SOURCE" }"#),
            "the source's queued message is not the fork's to answer: {forked}"
        );
        assert!(
            !forked.contains("Forked("),
            "a fork must not carry its own creation marker: {forked}"
        );
        provider.release();
    }

    /// A provider whose *first* summarising call never answers, and whose every
    /// other call ends the turn with plain text.
    ///
    /// The summary is told apart by the message id agentcore gives it, which is
    /// the only thing that distinguishes it on the wire: it carries no system
    /// prompt and no tools either, but a stalled ordinary turn would do just as
    /// well for those.
    struct StallsFirstSummary {
        summaries: std::sync::atomic::AtomicUsize,
    }

    impl StallsFirstSummary {
        fn new() -> Self {
            Self {
                summaries: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl horsie_agentcore::LlmProvider for StallsFirstSummary {
        fn model_id(&self) -> &str {
            "mock"
        }

        async fn complete(
            &self,
            _request: horsie_agentcore::CompletionRequest<'_>,
            message_id: &str,
            _events: &dyn horsie_agentcore::EventSink,
        ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
            if message_id == "compaction"
                && self
                    .summaries
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            {
                // The process died here. Nothing answers, ever.
                std::future::pending::<()>().await;
            }
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "what was said before".to_string(),
                    },
                )],
                stop_reason: horsie_agentcore::StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }
    }

    /// The seed always frames where the fork came from; only a summary fork
    /// carries a summary, because only it discarded the history.
    #[test]
    fn the_seed_frames_the_source_and_carries_a_summary_only_when_there_is_one() {
        let copy = seed_text("Migrate the journal", "");
        assert!(copy.contains("forked from \"Migrate the journal\""));
        assert!(copy.contains("set_session_title"));
        assert!(!copy.contains("# Summary"));

        let summarised = seed_text("Migrate the journal", "We chose sqlx::Any.");
        assert!(summarised.contains("# Summary"));
        assert!(summarised.contains("We chose sqlx::Any."));
    }
}
