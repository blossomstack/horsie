//! `set_timer`, `list_timers`, `cancel_timer`: an agent asking to be woken.
//!
//! An armed timer is the one thing an agent holds that is neither a question
//! nor a child: nothing is owed to it and nobody will answer it, and yet a turn
//! ending is not the agent finishing while one is armed. [`holds`] is that
//! fact, and the actor asks the capability that owns the records for it rather
//! than reading a field of its own.
//!
//! # The one thing this file cannot do for itself
//!
//! Every other answer here is a decision about state this file holds. Arming is
//! a request for *time to pass*: wake me in N seconds. That is [`Wake`] — the
//! actor spawns the sleep and sends the id back when it elapses.
//!
//! A wake is **not journaled**, and that is deliberate. The durable fact is the
//! [`TimerRecord`] — its `fire_at_unix_ms` is stamped once, when the timer is
//! armed, and carried on the event — so a sleep is only ever a consequence of
//! it. Journaling both would be two records of one fact that could disagree
//! after a restart. Instead [`reloaded`] re-issues a wake for every timer still
//! armed, with its *remaining* delay, which is exactly what recovery did before
//! any of this was a capability.
//!
//! A sleep cannot be cancelled once spawned, so a cancelled timer's sleep still
//! arrives. It is dropped by not being recognised: [`woke`] answers `None` for
//! an id this file no longer holds, and a stale wake reaches nothing.
//!
//! # Ids are derived, never generated
//!
//! The queue item a firing produces is `{timer id}:{fire count}`. Both halves
//! come off the record, because a journal replay has to reproduce the id the
//! live run wrote — the inbox dedupes on it. Nothing in a fold reads a clock or
//! mints a uuid: `fire_at_unix_ms` is computed here, in the decision, and
//! travels on the event.

use super::Mailbox;
use crate::agent_loop::timers::{
    CancelSelector, TimerId, TimerKind, TimerRecord, cancel_timer_spec, list_timers_spec,
    set_timer_spec,
};
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::runners::loading::AgentFacts;
use horsie_agentcore::Toolbox;
use horsie_models::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// What [`TimerState::carried_state`] writes above the armed timers it lists.
///
/// Named because it is read as well as written: a compaction boundary carries
/// this block into the next conversation, and it is also the one thing outside
/// this capability that can tell whether an agent still holds a timer at all.
pub const CARRIED_HEADER: &str = "Armed timers:";

/// The tool that arms one.
pub const SET_TOOL: &str = "set_timer";
/// The tool that reads them back.
pub const LIST_TOOL: &str = "list_timers";
/// The tool that removes one, or all of them.
pub const CANCEL_TOOL: &str = "cancel_timer";

/// The permission to arm one. What is armed is [`TimerState`], on
/// [`AgentState`](crate::agent_loop::AgentState).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimersCapability;

/// The timers this agent has armed.
///
/// The field is private to this file, so nothing else can add a record, remove
/// one, or move a fire time — the three things a replay has to reproduce
/// exactly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimerState {
    #[serde(default)]
    armed: Vec<TimerRecord>,
}

impl TimerState {
    /// A timer was armed, with its fire time already stamped on the event.
    pub(crate) fn arm(&mut self, record: TimerRecord) {
        self.armed.push(record);
    }

    /// These timers were removed.
    pub(crate) fn cancel(&mut self, ids: &[TimerId]) {
        self.armed.retain(|t| !ids.contains(&t.id));
    }

    /// A timer fired. `next_fire_at_unix_ms` re-arms a recurring one at the
    /// time the decision computed; `None` removes a one-shot.
    pub(crate) fn fired(&mut self, id: &TimerId, next_fire_at_unix_ms: Option<u64>) {
        match next_fire_at_unix_ms {
            Some(next) => {
                if let Some(t) = self.armed.iter_mut().find(|t| &t.id == id) {
                    t.fire_at_unix_ms = next;
                    t.fire_count += 1;
                }
            }
            None => self.armed.retain(|t| &t.id != id),
        }
    }

    /// Which armed timers a selector names, in list order.
    fn select(&self, selector: &CancelSelector) -> Vec<TimerId> {
        match selector {
            CancelSelector::All => self.armed.iter().map(|t| t.id.clone()).collect(),
            CancelSelector::One(id) => self
                .armed
                .iter()
                .filter(|t| &t.id == id)
                .map(|t| t.id.clone())
                .collect(),
        }
    }
}

#[cfg(test)]
/// What this state holds, for the tests that assert on it.
///
/// `#[cfg(test)]` because nothing in production reads it: the decisions that
/// need it are in this file and take `&self`. An accessor kept for a caller
/// that does not exist is how a private field stops being private.
impl TimerState {
    /// The timers still armed, oldest first.
    #[must_use]
    pub(crate) fn armed(&self) -> &[TimerRecord] {
        &self.armed
    }
}

/// One sleep this capability asked for.
///
/// The one thing it cannot do for itself: everything else is a decision about
/// state it holds, and this is a request for time to pass. The actor spawns the
/// sleep and sends the id back when it elapses.
///
/// **Not journaled**, in either direction. The durable fact is the armed
/// timer's own `fire_at_unix_ms`; a sleep is only ever a consequence of it, and
/// two records of one fact could disagree. A load re-issues every wake from
/// what is armed, with its *remaining* delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Wake {
    pub id: String,
    pub after_secs: u64,
}

/// What a call to `set_timer` came to.
#[derive(Debug)]
pub(crate) enum Armed {
    /// The model gets a tool *error*: `is_error` is read by agentcore's loop
    /// detector and the nudge budget.
    Refused(String),
    Armed {
        record: TimerRecord,
        wake: Wake,
        /// What the model is told, rendered the way agentcore forwards a result.
        told: String,
    },
}

/// The model called `set_timer`.
///
/// The fire time is stamped here, once, and travels on the event: a fold that
/// recomputed it would move every timer forward on every replay.
pub(crate) fn arm(input: &Value) -> Armed {
    let kind = match input.get("kind").and_then(Value::as_str) {
        Some("one_shot") => TimerKind::OneShot,
        Some("recurring") => TimerKind::Recurring,
        Some(_) | None => {
            return Armed::Refused(format!("{SET_TOOL}.kind must be 'one_shot' or 'recurring'"));
        }
    };
    let Some(after_secs) = input
        .get("after_secs")
        .and_then(Value::as_u64)
        .filter(|n| *n >= 1)
    else {
        return Armed::Refused(format!("{SET_TOOL}.after_secs must be an integer >= 1"));
    };
    let Some(message) = input
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Armed::Refused(format!("{SET_TOOL}.message must be a non-empty string"));
    };
    let label = input
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let record = TimerRecord::arm(
        label,
        message,
        kind,
        Duration::from_secs(after_secs),
        now_ms(),
    );
    let wake = Wake {
        id: record.id.0.clone(),
        after_secs,
    };
    let told = rendered(&json!({ "timer_id": record.id.0 }));
    Armed::Armed { record, wake, told }
}

/// The model called `list_timers`.
///
/// Reading them back changes nothing, so this is an answer and only an answer:
/// there is no event for the actor's arm to journal.
pub(crate) fn list(state: &TimerState) -> String {
    let now = now_ms();
    let views: Vec<_> = state.armed.iter().map(|t| t.view(now)).collect();
    rendered(&views)
}

/// What a call to `cancel_timer` came to.
#[derive(Debug)]
pub(crate) enum Cancelled {
    Refused(String),
    /// Cancelling nothing journals nothing and is still an answer, so `ids` may
    /// be empty.
    Cancelled {
        ids: Vec<TimerId>,
        told: String,
    },
}

/// The model called `cancel_timer`.
///
/// Cancelling nothing journals nothing and is still an answer: a model that
/// names a timer that has already fired has not made an error worth flagging,
/// and the empty list says so.
pub(crate) fn cancel(state: &TimerState, input: &Value) -> Cancelled {
    let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
        CancelSelector::All
    } else if let Some(id) = input.get("id").and_then(Value::as_str) {
        CancelSelector::One(TimerId(id.to_string()))
    } else {
        return Cancelled::Refused(format!("{CANCEL_TOOL} requires 'id' or 'all': true"));
    };
    let ids = state.select(&selector);
    let names: Vec<&str> = ids.iter().map(|i| i.0.as_str()).collect();
    let told = rendered(&json!({ "cancelled": names }));
    Cancelled::Cancelled { ids, told }
}

/// A timer fired.
#[derive(Debug)]
pub(crate) struct Fired {
    pub id: TimerId,
    /// The re-armed fire time for a recurring timer, so the fold stays pure;
    /// `None` removes a one-shot.
    pub next_fire_at_unix_ms: Option<u64>,
    /// The firing, as one more thing addressed to this agent. It waits in the
    /// queue where everything else does, which is what makes a timer firing
    /// mid-run harmless.
    pub item: Incoming,
    /// A recurring timer's next sleep.
    pub wake: Option<Wake>,
}

/// A sleep elapsed. `None` for an id this capability no longer holds, which is
/// the whole of how a cancelled timer's sleep is dropped — the sleep itself
/// cannot be called back.
pub(crate) fn woke(state: &TimerState, id: &str) -> Option<Fired> {
    let record = state.armed.iter().find(|t| t.id.0 == id)?;
    let display_count = record.fire_count + 1;
    // Recurring re-arms from now; a one-shot is removed. Computed here and
    // carried on the event, because a fold may not read a clock.
    let next_fire_at_unix_ms = match record.kind {
        TimerKind::Recurring => {
            Some(now_ms().saturating_add(record.interval_secs.saturating_mul(1000)))
        }
        TimerKind::OneShot => None,
    };
    let wake = match record.kind {
        TimerKind::Recurring => Some(Wake {
            id: record.id.0.clone(),
            after_secs: record.interval_secs,
        }),
        TimerKind::OneShot => None,
    };
    Some(Fired {
        id: record.id.clone(),
        next_fire_at_unix_ms,
        item: Incoming::Timer {
            // Derived from the timer and its fire count, never generated:
            // replay must land the id the live run wrote, which a uuid
            // could not.
            id: format!("{}:{display_count}", record.id),
            message: record.wake_message(display_count),
        },
        wake,
    })
}

/// Every timer still armed, asked for again — with its *remaining* delay, not
/// its original interval. Empty when nothing is armed.
///
/// The crash window's counterpart for a wake: a sleep lives in the dead process
/// and nothing survives it, so the fold is the only record that a timer is due.
/// A five-minute timer armed four minutes ago is due in one, and re-arming it
/// for five would silently move it.
pub(crate) fn reloaded(state: &TimerState) -> Vec<Wake> {
    let now = now_ms();
    state
        .armed
        .iter()
        .map(|t| Wake {
            id: t.id.0.clone(),
            after_secs: t.remaining(now).as_secs(),
        })
        .collect()
}

/// Every timer, dropped: the agent said its work is done, so nothing is left
/// for one to wake. `None` when nothing is armed.
///
/// Left armed, a reload would re-arm them and drop a wake into a finished
/// agent's queue. Dropped rather than refused at the tool boundary, where the
/// agent's own timers are invisible to it.
pub(crate) fn concluded(state: &TimerState) -> Option<Vec<TimerId>> {
    match state.armed.is_empty() {
        true => None,
        false => Some(state.select(&CancelSelector::All)),
    }
}

/// Why this turn's end is not the agent finishing, when a timer is still armed.
///
/// Invariant 6, asked directly rather than merged out of a broadcast: the timer
/// is what will start the next turn.
pub(crate) fn holds(state: &TimerState) -> Option<String> {
    match state.armed.is_empty() {
        true => None,
        false => Some(format!("{} timer(s) still armed", state.armed.len())),
    }
}

/// A tool result, rendered the way agentcore forwards one.
///
/// Compact JSON, which is what a `serde_json::Value` result became before these
/// tools answered with a string — a string is forwarded verbatim, so the bytes
/// the model sees are the same either way.
fn rendered<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|e| format!("could not render the timer result: {e}"))
}

impl TimersCapability {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The three tools, each paired with the command a call to it becomes.
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![
            ClaimedTool::new(set_timer_spec(), |input, to| AgentCommand::TimerArm {
                input,
                answering: to,
            }),
            ClaimedTool::new(list_timers_spec(), |_input, to| AgentCommand::TimerList {
                answering: to,
            }),
            ClaimedTool::new(cancel_timer_spec(), |input, to| AgentCommand::TimerCancel {
                input,
                answering: to,
            }),
        ]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl TimersCapability {
    pub fn name(&self) -> &'static str {
        "timers"
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(), mailbox)
    }
}

impl TimerState {
    /// An armed timer is exact and invisible: the only trace of it in the
    /// history is the `set_timer` call a compaction summarises away.
    #[must_use]
    pub fn carried_state(&self) -> Option<String> {
        if self.armed.is_empty() {
            return None;
        }
        let mut block = String::from(CARRIED_HEADER);
        for t in &self.armed {
            block.push_str(&format!(
                "\n- {} ({}) fires at {}ms: {}",
                t.id,
                t.label,
                t.fire_at_unix_ms,
                match t.message.is_empty() {
                    true => "(no message)",
                    false => t.message.as_str(),
                }
            ));
        }
        Some(block)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, facts};
    use super::*;
    use crate::agent_loop::capabilities::Capability;
    use crate::agent_loop::state::AgentDomainEvent;

    /// Journal one event, the way the actor's arm does. A capability that
    /// decided something has not yet changed anything; this is the step that
    /// makes it true.
    fn fold(timers: TimerState, event: AgentDomainEvent) -> TimerState {
        crate::agent_loop::AgentState {
            timers,
            ..Default::default()
        }
        .apply(event)
        .timers
    }

    /// An agent holding one timer armed from `input`, folded the way the actor
    /// does.
    fn armed_one(input: Value) -> TimerState {
        let Armed::Armed { record, .. } = arm(&input) else {
            panic!("a well-formed set_timer arms a timer");
        };
        fold(
            TimerState::default(),
            AgentDomainEvent::TimerArmed { record },
        )
    }

    /// What the model was told, read back as JSON.
    fn told_json(told: &str) -> Value {
        serde_json::from_str(told).expect("a timer answer is JSON")
    }

    /// The event the actor's arm journals for a firing.
    fn fired_event(fired: &Fired) -> AgentDomainEvent {
        AgentDomainEvent::TimerFired {
            id: fired.id.clone(),
            next_fire_at_unix_ms: fired.next_fire_at_unix_ms,
        }
    }

    fn one_shot(after_secs: u64) -> Value {
        json!({
            "kind": "one_shot",
            "after_secs": after_secs,
            "label": "check back",
            "message": "see whether CI went green",
        })
    }

    fn recurring(after_secs: u64) -> Value {
        json!({
            "kind": "recurring",
            "after_secs": after_secs,
            "label": "nightly",
            "message": "re-run the sweep",
        })
    }

    #[test]
    fn it_advertises_the_three_timer_tools() {
        assert_eq!(
            advertised_by(&Capability::Timers(TimersCapability), &facts()),
            vec![SET_TOOL, LIST_TOOL, CANCEL_TOOL]
        );
    }

    /// Arming hands the actor the record to journal with its fire time already
    /// stamped, answers the model with the id, and asks for the one thing the
    /// capability cannot do for itself.
    #[test]
    fn arming_journals_the_record_and_asks_to_be_woken() {
        let Armed::Armed { record, wake, told } = arm(&one_shot(3600)) else {
            panic!("a well-formed set_timer arms a timer");
        };
        assert!(
            record.fire_at_unix_ms > now_ms(),
            "the fire time is stamped in the decision, not left for the fold"
        );
        assert_eq!(record.interval_secs, 3600);
        assert_eq!(told_json(&told)["timer_id"], record.id.0.as_str());
        assert_eq!(
            wake,
            Wake {
                id: record.id.0.clone(),
                after_secs: 3600
            }
        );
    }

    /// A malformed call is an error result rather than a plain one, and carries
    /// no record: no timer was armed, so the actor journals nothing.
    #[test]
    fn a_malformed_set_timer_is_refused_and_journals_nothing() {
        for input in [
            json!({"kind": "sometimes", "after_secs": 10, "message": "x"}),
            json!({"kind": "one_shot", "after_secs": 0, "message": "x"}),
            json!({"kind": "one_shot", "after_secs": 10}),
        ] {
            match arm(&input) {
                Armed::Refused(_) => {}
                Armed::Armed { record, .. } => {
                    panic!("{input} was not refused, and armed {record:?}")
                }
            }
        }
    }

    /// The fired inbox item's id is `{timer}:{fire count}` and is *derived*,
    /// never generated: the queue dedupes on it, and a journal replay has to
    /// land the same id the live run wrote — which a uuid could not.
    #[test]
    fn a_wake_queues_an_item_whose_id_is_derived_from_the_fire_count() {
        let timers = armed_one(recurring(60));
        let id = timers.armed()[0].id.0.clone();

        let first = woke(&timers, &id).expect("its own wake");
        let Incoming::Timer { id: item, message } = &first.item else {
            panic!("a timer's wake is queued as a timer");
        };
        assert_eq!(*item, format!("{id}:1"));
        assert!(message.contains("re-run the sweep"));
        let timers = fold(timers, fired_event(&first));

        // And the second fire numbers on from the count the fold carried, so
        // replaying both reproduces both ids.
        let second = woke(&timers, &id).expect("its own wake");
        let Incoming::Timer { id: item, .. } = &second.item else {
            panic!("a timer's wake is queued as a timer");
        };
        assert_eq!(*item, format!("{id}:2"));
    }

    /// A recurring timer asks to be woken again; a one-shot does not, and is
    /// gone from the fold.
    #[test]
    fn a_recurring_timer_re_arms_and_a_one_shot_is_removed() {
        let timers = armed_one(recurring(60));
        let id = timers.armed()[0].id.0.clone();
        let fired = woke(&timers, &id).expect("its own wake");
        assert_eq!(
            fired.wake,
            Some(Wake {
                id: id.clone(),
                after_secs: 60
            })
        );
        let timers = fold(timers, fired_event(&fired));
        assert_eq!(timers.armed().len(), 1, "a recurring timer stays armed");
        assert_eq!(timers.armed()[0].fire_count, 1);

        let timers = armed_one(one_shot(60));
        let id = timers.armed()[0].id.0.clone();
        let fired = woke(&timers, &id).expect("its own wake");
        assert_eq!(fired.wake, None, "a one-shot asks for nothing more");
        let timers = fold(timers, fired_event(&fired));
        assert!(timers.armed().is_empty());
    }

    /// A sleep cannot be cancelled once it is spawned, so a cancelled timer's
    /// sleep still arrives. Not recognising the id is how it is dropped — and
    /// `None` is what lets the actor tell that from a bug.
    #[test]
    fn a_wake_for_a_cancelled_timer_is_not_claimed() {
        let timers = armed_one(one_shot(60));
        let id = timers.armed()[0].id.0.clone();
        let Cancelled::Cancelled { ids, told } = cancel(&timers, &json!({"id": id})) else {
            panic!("naming a timer is a well-formed cancel");
        };
        assert_eq!(told_json(&told)["cancelled"], json!([id.as_str()]));
        let timers = fold(timers, AgentDomainEvent::TimersCancelled { ids });
        assert!(timers.armed().is_empty());
        assert!(
            woke(&timers, &id).is_none(),
            "a stale sleep was claimed, and would have fired a cancelled timer"
        );
        assert!(
            woke(&timers, "someone-else").is_none(),
            "a wake for an id this agent never armed was claimed"
        );
    }

    /// Cancelling everything names every timer for one event; cancelling
    /// nothing is an answer with no ids at all, because nothing happened.
    #[test]
    fn cancel_all_removes_every_timer_and_cancelling_nothing_journals_nothing() {
        let timers = armed_one(one_shot(60));
        let Armed::Armed { record, .. } = arm(&recurring(60)) else {
            panic!("a well-formed set_timer arms a timer");
        };
        let timers = fold(timers, AgentDomainEvent::TimerArmed { record });
        assert_eq!(timers.armed().len(), 2);

        let Cancelled::Cancelled { ids, .. } = cancel(&timers, &json!({"all": true})) else {
            panic!("'all': true is a well-formed cancel");
        };
        assert_eq!(ids.len(), 2);
        let timers = fold(timers, AgentDomainEvent::TimersCancelled { ids });
        assert!(timers.armed().is_empty());

        let Cancelled::Cancelled { ids, told } = cancel(&timers, &json!({"all": true})) else {
            panic!("'all': true is a well-formed cancel");
        };
        assert!(ids.is_empty(), "cancelling nothing is not a fact");
        assert_eq!(told_json(&told)["cancelled"], json!([]));
    }

    /// `list_timers` is the reliable source of truth for cancelling, so it has
    /// to report the remaining delay rather than the configured interval. It
    /// answers and nothing else: there is no event for the actor to journal.
    #[test]
    fn listing_reports_what_is_armed_and_journals_nothing() {
        let timers = armed_one(recurring(3600));
        let views = told_json(&list(&timers));
        assert_eq!(views[0]["label"], "nightly");
        assert_eq!(views[0]["kind"], "recurring");
        assert!(views[0]["fires_in_secs"].as_u64().unwrap() <= 3600);
    }

    /// A sleep dies with the process that spawned it, so a load re-issues one
    /// for every timer still armed — with the *remaining* delay. Re-arming for
    /// the original interval would silently push every timer forward by a
    /// restart's worth of time. A load journals nothing, which is why it
    /// returns wakes and no event at all.
    #[test]
    fn a_load_re_arms_every_timer_with_its_remaining_delay() {
        let timers = fold(
            TimerState::default(),
            AgentDomainEvent::TimerArmed {
                record: TimerRecord {
                    id: TimerId("t-1".into()),
                    label: "nightly".into(),
                    message: "re-run the sweep".into(),
                    kind: TimerKind::Recurring,
                    interval_secs: 86_400,
                    fire_at_unix_ms: now_ms() + 60_000,
                    fire_count: 3,
                },
            },
        );
        let wakes = reloaded(&timers);
        let [wake] = wakes.as_slice() else {
            panic!("expected one wake, got {wakes:?}");
        };
        assert_eq!(wake.id, "t-1");
        assert!(
            (55..=60).contains(&wake.after_secs),
            "re-armed for {}s, not the ~60s remaining",
            wake.after_secs
        );
    }

    /// An agent with nothing armed asks for nothing on a load, so an ordinary
    /// reload does not spawn a task per capability.
    #[test]
    fn a_load_with_nothing_armed_asks_for_nothing() {
        assert!(reloaded(&TimerState::default()).is_empty());
    }

    /// Invariant 6 for a timer: a turn that ends with one armed is not the
    /// agent finishing, because the timer is what starts the next turn.
    #[test]
    fn a_turn_ending_with_a_timer_armed_is_held() {
        let holding = armed_one(one_shot(3600));
        assert_eq!(
            holds(&holding),
            Some("1 timer(s) still armed".to_string()),
            "a step holding a timer would be nudged into submitting a result it does not have"
        );
        assert!(
            holds(&TimerState::default()).is_none(),
            "an agent with nothing armed holds nothing"
        );
    }

    /// Submitting a result says the work is done, so an armed timer is moot.
    /// Left armed it would be re-armed by the next load and drop a wake into a
    /// finished agent's queue.
    #[test]
    fn concluding_cancels_every_armed_timer() {
        let timers = armed_one(one_shot(3600));
        let ids = concluded(&timers).expect("a timer to drop");
        let timers = fold(timers, AgentDomainEvent::TimersCancelled { ids });
        assert!(timers.armed().is_empty());
        assert!(
            concluded(&timers).is_none(),
            "an agent with nothing armed has nothing to drop"
        );
    }

    /// The armed timers have to survive the round trip a reload takes, or an
    /// agent comes back with nothing to wake it.
    #[test]
    fn armed_timers_survive_the_journal_round_trip() {
        let state = crate::agent_loop::AgentState {
            timers: armed_one(recurring(60)),
            ..Default::default()
        };
        let written = serde_json::to_string(&state).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(back.timers.armed().len(), 1);
        assert_eq!(
            back.timers.armed()[0].label,
            "nightly",
            "a reload that lost the armed timers comes back with nothing to wake it"
        );
    }

    /// Nothing armed carries nothing, so a session that never set a timer gets
    /// no paragraph saying so.
    #[test]
    fn nothing_armed_carries_nothing() {
        assert_eq!(TimerState::default().carried_state(), None);
    }
}
