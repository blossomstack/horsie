//! `set_timer`, `list_timers`, `cancel_timer`: an agent asking to be woken.
//!
//! An armed timer is the one thing an agent holds that is neither a question
//! nor a child: nothing is owed to it and nobody will answer it, and yet a turn
//! ending is not the agent finishing while one is armed — see
//! [`Act::Hold`]. So the same capability that owns the records answers the turn
//! boundary, which is how that fact stopped being a field the actor read.
//!
//! # The verb the trait did not have
//!
//! Every other capability decides from something that happened. This one has to
//! ask for something to happen: *wake me in N seconds*. That is [`Act::Wake`],
//! and [`Msg::Woke`] is what comes back.
//!
//! A wake is **not journaled**, and that is deliberate. The durable fact is the
//! [`TimerRecord`] — its `fire_at_unix_ms` is stamped once, when the timer is
//! armed, and carried on the event — so a sleep is only ever a consequence of
//! it. Journaling both would be two records of one fact that could disagree
//! after a restart. Instead [`Msg::Loaded`] re-issues a wake for every timer
//! still armed, with its *remaining* delay, which is exactly what recovery did
//! before any of this was a capability.
//!
//! A sleep cannot be cancelled once spawned, so a cancelled timer's sleep still
//! arrives. It is dropped by not being recognised: [`Msg::Woke`] is offered
//! around, this capability answers `None` for an id it no longer holds, and a
//! stale wake reaches nothing.
//!
//! # Ids are derived, never generated
//!
//! The queue item a firing produces is `{timer id}:{fire count}`. Both halves
//! come off the record, because a journal replay has to reproduce the id the
//! live run wrote — the inbox dedupes on it. Nothing in a fold reads a clock or
//! mints a uuid: `fire_at_unix_ms` is computed here, in the decision, and
//! travels on the event.

use super::{Act, CapCommand, CapEvent, CapSlice, Decision, Mailbox, Msg, TurnEvent};
use crate::agent_loop::Incoming;
use crate::agent_loop::timers::{
    CancelSelector, TimerId, TimerKind, TimerRecord, cancel_timer_spec, list_timers_spec,
    set_timer_spec,
};
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::sessions::runners::loading::AgentFacts;
use horsie_agentcore::Toolbox;
use horsie_models::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// What [`super::Capability::carried_state`] writes above the armed timers it lists.
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

/// What the model asked this capability to do.
///
/// One arm per tool, decided by the layer that claimed the name. The three used
/// to be told apart by a name match here, and the name is what no longer
/// travels.
pub enum Command {
    /// `set_timer`.
    Arm { input: Value },
    /// `list_timers`. No input: reading them back takes no arguments.
    List,
    /// `cancel_timer`.
    Cancel { input: Value },
}

/// What this capability records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// A timer was armed, with its fire time already computed.
    Armed { record: TimerRecord },
    /// These timers were removed.
    Cancelled { ids: Vec<TimerId> },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for
    /// a recurring timer, so the fold stays pure; `None` removes a one-shot.
    Fired {
        id: TimerId,
        next_fire_at_unix_ms: Option<u64>,
    },
}

/// One agent's armed timers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimersCapability {
    armed: Vec<TimerRecord>,
}

impl TimersCapability {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The timers still armed, oldest first.
    #[must_use]
    pub fn armed(&self) -> &[TimerRecord] {
        &self.armed
    }

    /// The model called `set_timer`.
    ///
    /// The fire time is stamped here, once, and travels on the event: a fold
    /// that recomputed it would move every timer forward on every replay.
    fn arm(&self, call: &str, input: &Value) -> Decision {
        let kind = match input.get("kind").and_then(Value::as_str) {
            Some("one_shot") => TimerKind::OneShot,
            Some("recurring") => TimerKind::Recurring,
            Some(_) | None => {
                return Decision::refuse(
                    call,
                    format!("{SET_TOOL}.kind must be 'one_shot' or 'recurring'"),
                );
            }
        };
        let Some(after_secs) = input
            .get("after_secs")
            .and_then(Value::as_u64)
            .filter(|n| *n >= 1)
        else {
            return Decision::refuse(
                call,
                format!("{SET_TOOL}.after_secs must be an integer >= 1"),
            );
        };
        let Some(message) = input
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
        else {
            return Decision::refuse(
                call,
                format!("{SET_TOOL}.message must be a non-empty string"),
            );
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
        let id = record.id.clone();
        Decision::record(vec![CapEvent::Timer(Event::Armed { record })])
            .then(answer(call, &json!({ "timer_id": id.0 })))
            .then(Act::Wake {
                id: id.0,
                after_secs,
            })
    }

    /// The model called `list_timers`.
    fn list(&self, call: &str) -> Decision {
        let now = now_ms();
        let views: Vec<_> = self.armed.iter().map(|t| t.view(now)).collect();
        Decision::default().then(answer(call, &views))
    }

    /// The model called `cancel_timer`.
    ///
    /// Cancelling nothing journals nothing and is still an answer: a model that
    /// names a timer that has already fired has not made an error worth
    /// flagging, and the empty list says so.
    fn cancel(&self, call: &str, input: &Value) -> Decision {
        let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
            CancelSelector::All
        } else if let Some(id) = input.get("id").and_then(Value::as_str) {
            CancelSelector::One(TimerId(id.to_string()))
        } else {
            return Decision::refuse(call, format!("{CANCEL_TOOL} requires 'id' or 'all': true"));
        };
        let ids = self.select(&selector);
        let names: Vec<&str> = ids.iter().map(|i| i.0.as_str()).collect();
        let answered = Decision::default().then(answer(call, &json!({ "cancelled": names })));
        match ids.is_empty() {
            true => answered,
            false => Decision {
                events: vec![CapEvent::Timer(Event::Cancelled { ids })],
                ..answered
            },
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

    /// A sleep elapsed.
    ///
    /// `None` for an id this capability no longer holds, which is the whole of
    /// how a cancelled timer's sleep is dropped — the sleep itself cannot be
    /// called back.
    ///
    /// The wake goes in the queue rather than starting a turn: a firing is one
    /// more thing addressed to this agent, and it waits where everything else
    /// does. That is what makes a timer firing mid-run harmless.
    fn woke(&self, id: &str) -> Option<Decision> {
        let record = self.armed.iter().find(|t| t.id.0 == id)?;
        let display_count = record.fire_count + 1;
        // Recurring re-arms from now; a one-shot is removed. Computed here and
        // carried on the event, because a fold may not read a clock.
        let next_fire_at_unix_ms = match record.kind {
            TimerKind::Recurring => {
                Some(now_ms().saturating_add(record.interval_secs.saturating_mul(1000)))
            }
            TimerKind::OneShot => None,
        };
        let decision = Decision::record(vec![CapEvent::Timer(Event::Fired {
            id: record.id.clone(),
            next_fire_at_unix_ms,
        })])
        .then(Act::Enqueue {
            item: Incoming::Timer {
                // Derived from the timer and its fire count, never generated:
                // replay must land the id the live run wrote, which a uuid
                // could not.
                id: format!("{}:{display_count}", record.id),
                message: record.wake_message(display_count),
            },
        });
        Some(match record.kind {
            TimerKind::Recurring => decision.then(Act::Wake {
                id: record.id.0.clone(),
                after_secs: record.interval_secs,
            }),
            TimerKind::OneShot => decision,
        })
    }

    /// Every timer still armed, asked for again.
    ///
    /// The crash window's counterpart for a wake: a sleep lives in the dead
    /// process and nothing survives it, so the fold is the only record that a
    /// timer is due. Re-issued with the *remaining* delay rather than the
    /// original interval — a five-minute timer armed four minutes ago is due in
    /// one, and re-arming it for five would silently move it.
    fn reloaded(&self) -> Option<Decision> {
        if self.armed.is_empty() {
            return None;
        }
        let now = now_ms();
        Some(self.armed.iter().fold(Decision::default(), |d, t| {
            d.then(Act::Wake {
                id: t.id.0.clone(),
                after_secs: t.remaining(now).as_secs(),
            })
        }))
    }
}

/// A tool result, rendered the way agentcore forwards one.
///
/// Compact JSON, which is what a `serde_json::Value` result became before these
/// tools answered through [`Act::Answer`] — a string is forwarded verbatim, so
/// the bytes the model sees are the same either way.
fn answer<T: Serialize>(call: &str, value: &T) -> Act {
    Act::Answer {
        call: call.to_string(),
        text: serde_json::to_string(value)
            .unwrap_or_else(|e| format!("could not render the timer result: {e}")),
    }
}

impl TimersCapability {
    /// The three tools, each paired with the command a call to it becomes.
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![
            ClaimedTool::new(set_timer_spec(), |input, to| {
                CapCommand::Timers(Command::Arm { input }, to)
            }),
            ClaimedTool::new(list_timers_spec(), |_input, to| {
                CapCommand::Timers(Command::List, to)
            }),
            ClaimedTool::new(cancel_timer_spec(), |input, to| {
                CapCommand::Timers(Command::Cancel { input }, to)
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

    pub fn command(&self, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::Timers(cmd, to) = cmd else {
            return None;
        };
        Some(match cmd {
            Command::Arm { input } => self.arm(&to.call, input),
            Command::List => self.list(&to.call),
            Command::Cancel { input } => self.cancel(&to.call, input),
        })
    }

    pub fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::Woke { id } => self.woke(id),
            Msg::Loaded => self.reloaded(),
            // Invariant 6, for the one thing an agent holds that owes it
            // nothing: a turn ending with an armed timer is not the agent
            // finishing, because the timer is what will start the next one.
            Msg::Turn(TurnEvent::Ended) if !self.armed.is_empty() => {
                Some(Decision::default().then(Act::Hold {
                    note: format!("{} timer(s) still armed", self.armed.len()),
                }))
            }
            // Submitting a result says the work is done, which makes an armed
            // timer moot: nothing is left for it to wake, and a reload would
            // otherwise re-arm it and drop a wake into a finished agent's queue.
            // Dropped rather than refused at the tool boundary, where the
            // agent's own timers are invisible to it.
            Msg::Concluded if !self.armed.is_empty() => {
                Some(Decision::record(vec![CapEvent::Timer(Event::Cancelled {
                    ids: self.select(&CancelSelector::All),
                })]))
            }
            Msg::Turn(_)
            | Msg::Answer(_)
            | Msg::Child(_)
            | Msg::Reply(_)
            | Msg::Concluded
            | Msg::TurnProposed => None,
        }
    }

    pub fn apply(&mut self, event: &CapEvent) {
        let CapEvent::Timer(event) = event else {
            return;
        };
        match event {
            Event::Armed { record } => self.armed.push(record.clone()),
            Event::Cancelled { ids } => self.armed.retain(|t| !ids.contains(&t.id)),
            Event::Fired {
                id,
                next_fire_at_unix_ms,
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = self.armed.iter_mut().find(|t| &t.id == id) {
                        t.fire_at_unix_ms = *next;
                        t.fire_count += 1;
                    }
                }
                None => self.armed.retain(|t| &t.id != id),
            },
        }
    }

    /// An armed timer is exact and invisible: the only trace of it in the
    /// history is the `set_timer` call a compaction summarises away.
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

    pub fn save(&self) -> CapSlice {
        CapSlice::Timers(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, answering, facts, someone_elses};
    use super::*;
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    fn called(cap: &TimersCapability, cmd: Command) -> Decision {
        cap.command(&CapCommand::Timers(cmd, answering("t1")))
            .expect("timers own their commands")
    }

    fn set(cap: &TimersCapability, input: Value) -> Decision {
        called(cap, Command::Arm { input })
    }

    fn cancel(cap: &TimersCapability, input: Value) -> Decision {
        called(cap, Command::Cancel { input })
    }

    fn fold(cap: &mut TimersCapability, decision: &Decision) {
        for event in &decision.events {
            cap.apply(event);
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

    fn wake(decision: &Decision) -> Option<(String, u64)> {
        decision.acts.iter().find_map(|act| {
            let Act::Wake { id, after_secs } = act else {
                return None;
            };
            Some((id.clone(), *after_secs))
        })
    }

    fn answered(decision: &Decision) -> Value {
        for act in &decision.acts {
            if let Act::Answer { text, .. } = act {
                return serde_json::from_str(text).expect("a timer answer is JSON");
            }
        }
        panic!("expected an answer, got {:?}", decision.acts);
    }

    fn queued(decision: &Decision) -> Incoming {
        for act in &decision.acts {
            if let Act::Enqueue { item } = act {
                return item.clone();
            }
        }
        panic!("expected a queued wake, got {:?}", decision.acts);
    }

    /// Arm one and fold it, the way the actor does.
    fn armed_one(input: Value) -> TimersCapability {
        let mut cap = TimersCapability::new();
        let decision = set(&cap, input);
        fold(&mut cap, &decision);
        cap
    }

    #[test]
    fn it_advertises_the_three_timer_tools() {
        assert_eq!(
            advertised_by(&Capability::Timers(TimersCapability::new()), &facts()),
            vec![SET_TOOL, LIST_TOOL, CANCEL_TOOL]
        );
    }

    /// Arming journals the record with its fire time already stamped, answers
    /// the model with the id, and asks the actor for the one thing the
    /// capability cannot do for itself.
    #[test]
    fn arming_journals_the_record_and_asks_to_be_woken() {
        let cap = TimersCapability::new();
        let decision = set(&cap, one_shot(3600));
        let Some(CapEvent::Timer(Event::Armed { record })) = decision.events.first() else {
            panic!("expected an armed event, got {:?}", decision.events);
        };
        assert!(
            record.fire_at_unix_ms > now_ms(),
            "the fire time is stamped in the decision, not left for the fold"
        );
        assert_eq!(record.interval_secs, 3600);
        assert_eq!(answered(&decision)["timer_id"], record.id.0.as_str());
        assert_eq!(wake(&decision), Some((record.id.0.clone(), 3600)));
    }

    /// A malformed call is an error result rather than a plain one, and
    /// journals nothing: no timer was armed, so nothing happened.
    #[test]
    fn a_malformed_set_timer_is_refused_and_journals_nothing() {
        let cap = TimersCapability::new();
        for input in [
            json!({"kind": "sometimes", "after_secs": 10, "message": "x"}),
            json!({"kind": "one_shot", "after_secs": 0, "message": "x"}),
            json!({"kind": "one_shot", "after_secs": 10}),
        ] {
            let decision = set(&cap, input.clone());
            assert!(decision.events.is_empty(), "{input} armed something");
            assert!(
                decision
                    .acts
                    .iter()
                    .any(|a| matches!(a, Act::Refuse { .. })),
                "{input} was not refused: {:?}",
                decision.acts
            );
        }
    }

    /// The fired inbox item's id is `{timer}:{fire count}` and is *derived*,
    /// never generated: the queue dedupes on it, and a journal replay has to
    /// land the same id the live run wrote — which a uuid could not.
    #[test]
    fn a_wake_queues_an_item_whose_id_is_derived_from_the_fire_count() {
        let mut cap = armed_one(recurring(60));
        let id = cap.armed()[0].id.0.clone();

        let first = cap.handle(&Msg::Woke { id: &id }).expect("its own wake");
        let Incoming::Timer { id: item, message } = queued(&first) else {
            panic!("a timer's wake is queued as a timer");
        };
        assert_eq!(item, format!("{id}:1"));
        assert!(message.contains("re-run the sweep"));
        fold(&mut cap, &first);

        // And the second fire numbers on from the count the fold carried, so
        // replaying both reproduces both ids.
        let second = cap.handle(&Msg::Woke { id: &id }).expect("its own wake");
        let Incoming::Timer { id: item, .. } = queued(&second) else {
            panic!("a timer's wake is queued as a timer");
        };
        assert_eq!(item, format!("{id}:2"));
    }

    /// A recurring timer asks to be woken again; a one-shot does not, and is
    /// gone from the fold.
    #[test]
    fn a_recurring_timer_re_arms_and_a_one_shot_is_removed() {
        let mut cap = armed_one(recurring(60));
        let id = cap.armed()[0].id.0.clone();
        let fired = cap.handle(&Msg::Woke { id: &id }).expect("its own wake");
        assert_eq!(wake(&fired), Some((id.clone(), 60)));
        fold(&mut cap, &fired);
        assert_eq!(cap.armed().len(), 1, "a recurring timer stays armed");
        assert_eq!(cap.armed()[0].fire_count, 1);

        let mut cap = armed_one(one_shot(60));
        let id = cap.armed()[0].id.0.clone();
        let fired = cap.handle(&Msg::Woke { id: &id }).expect("its own wake");
        assert_eq!(wake(&fired), None, "a one-shot asks for nothing more");
        fold(&mut cap, &fired);
        assert!(cap.armed().is_empty());
    }

    /// A sleep cannot be cancelled once it is spawned, so a cancelled timer's
    /// sleep still arrives. Not recognising the id is how it is dropped — and
    /// `None` is what lets the actor tell that from a bug.
    #[test]
    fn a_wake_for_a_cancelled_timer_is_not_claimed() {
        let mut cap = armed_one(one_shot(60));
        let id = cap.armed()[0].id.0.clone();
        let cancelled = cancel(&cap, json!({"id": id}));
        assert_eq!(answered(&cancelled)["cancelled"], json!([id.as_str()]));
        fold(&mut cap, &cancelled);
        assert!(cap.armed().is_empty());
        assert!(
            cap.handle(&Msg::Woke { id: &id }).is_none(),
            "a stale sleep was claimed, and would have fired a cancelled timer"
        );
    }

    /// Cancelling everything is one event; cancelling nothing is an answer with
    /// no event at all, because nothing happened.
    #[test]
    fn cancel_all_removes_every_timer_and_cancelling_nothing_journals_nothing() {
        let mut cap = armed_one(one_shot(60));
        let second = set(&cap, recurring(60));
        fold(&mut cap, &second);
        assert_eq!(cap.armed().len(), 2);

        let all = cancel(&cap, json!({"all": true}));
        assert_eq!(all.events.len(), 1);
        fold(&mut cap, &all);
        assert!(cap.armed().is_empty());

        let again = cancel(&cap, json!({"all": true}));
        assert!(again.events.is_empty(), "cancelling nothing is not a fact");
        assert_eq!(answered(&again)["cancelled"], json!([]));
    }

    /// `list_timers` is the reliable source of truth for cancelling, so it has
    /// to report the remaining delay rather than the configured interval.
    #[test]
    fn listing_reports_what_is_armed_and_journals_nothing() {
        let cap = armed_one(recurring(3600));
        let listed = called(&cap, Command::List);
        assert!(listed.events.is_empty());
        let views = answered(&listed);
        assert_eq!(views[0]["label"], "nightly");
        assert_eq!(views[0]["kind"], "recurring");
        assert!(views[0]["fires_in_secs"].as_u64().unwrap() <= 3600);
    }

    /// A sleep dies with the process that spawned it, so a load re-issues one
    /// for every timer still armed — with the *remaining* delay. Re-arming for
    /// the original interval would silently push every timer forward by a
    /// restart's worth of time.
    #[test]
    fn a_load_re_arms_every_timer_with_its_remaining_delay() {
        let mut cap = TimersCapability::new();
        cap.apply(&CapEvent::Timer(Event::Armed {
            record: TimerRecord {
                id: TimerId("t-1".into()),
                label: "nightly".into(),
                message: "re-run the sweep".into(),
                kind: TimerKind::Recurring,
                interval_secs: 86_400,
                fire_at_unix_ms: now_ms() + 60_000,
                fire_count: 3,
            },
        }));
        let reloaded = cap.handle(&Msg::Loaded).expect("a timer to re-arm");
        assert!(reloaded.events.is_empty(), "a load journals nothing");
        let (id, after) = wake(&reloaded).expect("a wake");
        assert_eq!(id, "t-1");
        assert!(
            (55..=60).contains(&after),
            "re-armed for {after}s, not the ~60s remaining"
        );
    }

    /// An agent with nothing armed asks for nothing on a load, so an ordinary
    /// reload does not spawn a task per capability.
    #[test]
    fn a_load_with_nothing_armed_asks_for_nothing() {
        assert!(TimersCapability::new().handle(&Msg::Loaded).is_none());
    }

    /// Invariant 6 for a timer: a turn that ends with one armed is not the
    /// agent finishing, because the timer is what starts the next turn.
    #[test]
    fn a_turn_ending_with_a_timer_armed_is_held() {
        let cap = armed_one(one_shot(3600));
        assert!(
            cap.handle(&Msg::Turn(TurnEvent::Ended))
                .is_some_and(|d| d.acts.iter().any(|a| matches!(a, Act::Hold { .. }))),
            "a step holding a timer would be nudged into submitting a result it does not have"
        );
        assert!(
            TimersCapability::new()
                .handle(&Msg::Turn(TurnEvent::Ended))
                .is_none(),
            "an agent with nothing armed holds nothing"
        );
        for boundary in [TurnEvent::Began, TurnEvent::Failed, TurnEvent::Cancelled] {
            assert!(
                cap.handle(&Msg::Turn(boundary)).is_none(),
                "{boundary:?} is not the boundary a timer answers"
            );
        }
    }

    /// Submitting a result says the work is done, so an armed timer is moot.
    /// Left armed it would be re-armed by the next load and drop a wake into a
    /// finished agent's queue.
    #[test]
    fn concluding_cancels_every_armed_timer() {
        let mut cap = armed_one(one_shot(3600));
        let concluded = cap.handle(&Msg::Concluded).expect("a timer to drop");
        fold(&mut cap, &concluded);
        assert!(cap.armed().is_empty());
        assert!(
            cap.handle(&Msg::Concluded).is_none(),
            "an agent with nothing armed has nothing to drop"
        );
    }

    /// It claims its own three commands and nothing else.
    #[test]
    fn it_claims_nothing_but_its_own_commands() {
        let cap = armed_one(one_shot(60));
        assert!(cap.command(&someone_elses()).is_none());
        assert!(cap.handle(&Msg::Answer(&[])).is_none());
        assert!(cap.handle(&Msg::Woke { id: "someone-else" }).is_none());
    }

    /// The armed timers have to survive the round trip a reload takes, or an
    /// agent comes back with nothing to wake it.
    #[test]
    fn armed_timers_survive_the_journal_round_trip() {
        let cap = armed_one(recurring(60));
        let caps = Capabilities::new(vec![Capability::Timers(cap)]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::Timers(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.armed().len(), 1);
        assert_eq!(back.armed()[0].label, "nightly");
    }

    /// Nothing armed carries nothing, so a session that never set a timer gets
    /// no paragraph saying so.
    #[test]
    fn nothing_armed_carries_nothing() {
        assert_eq!(TimersCapability::new().carried_state(), None);
    }
}
