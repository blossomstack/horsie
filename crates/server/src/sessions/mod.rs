//! Interactive sessions: event-sourced actors on the shared `horsie-actor` core.
//!
//! `SessionSupervisor` (journal `session-supervisor/<account>`) owns the
//! registry of which sessions exist; a `SessionActor` (journal `session/<id>`)
//! is one of them, and hosts an `AgentActor` per agent (journal `agent/<id>`).
//! The two are separate clustered types rather than parent and child, so each
//! is placed on its own — see [`addressing`]. Recovery is lazy: a journal
//! replays when something addresses the actor it belongs to, and runtimes
//! respawn only on user action.

use serde::{Deserialize, Serialize};

pub mod addressing;
pub mod builder;
pub mod clock;
pub mod events;
pub mod forks;
pub mod runners;
pub mod session_actor;
pub mod spec;
pub mod subagents;
pub mod supervisor;
pub mod workflow;

/// How many times an agent has moved. Opaque: a reader compares it with the
/// last one it saw and re-reads when they differ, and that is all it means.
///
/// A counter rather than the `(tail_seq, delta_count)` pair this used to carry.
/// A reader now compares two values instead of holding a channel, and that pair
/// does not survive the comparison: an agent's first entry lands at sequence
/// zero with no deltas, which is bit-for-bit the value a reader starts from, so
/// the one thing a stream must never miss would have looked like no news at all.
pub type Revision = u64;

/// One agent's channel. `Arc` because the account's registry and the agent both
/// hold it, and the registry's copy is what keeps it alive across an offload.
pub type RevisionSender = std::sync::Arc<tokio::sync::watch::Sender<Revision>>;

type RevisionMap = std::collections::HashMap<String, RevisionSender>;

/// One agent's counter, on its way to a node that is not running it.
///
/// The counter and nothing else: what the agent actually did is read from its
/// state, by whoever the reader asks. That is what makes the bus's best-effort
/// delivery acceptable here — the value is absolute, so a frame that goes
/// missing is superseded by the next one rather than leaving a gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRevision {
    /// The wire agent id — `"main"`, or a subagent/step uuid.
    pub agent: String,
    pub revision: Revision,
}

/// A background task that ends when this is dropped.
///
/// The two here both block on something that will never return on its own — a
/// bus subscription, a channel nobody is writing to — so letting the handle go
/// would leak a task per session this node ever touched.
struct Relay(tokio::task::AbortHandle);

impl Drop for Relay {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
struct Registry {
    agents: RevisionMap,
    /// When a reader last asked about any of these. See `watched`.
    polled: Option<std::time::Instant>,
    /// One per agent whose counter this node moves: it forwards each new value
    /// onto the session's feed. Keyed by wire agent id so a second call for the
    /// same agent joins the existing one.
    forwarding: std::collections::HashMap<String, Relay>,
    /// Follows the feed, for a node that answers reads of agents it does not
    /// run. Present once something has asked.
    mirror: Option<Mirror>,
}

struct Mirror {
    _relay: Relay,
    /// Turns true once the subscription is live. A reader waits for it before
    /// its first read, so that nothing published after that read can be missed
    /// — a subscription started in the background would drop whatever was
    /// published while it was being set up, and the reader would then be a turn
    /// behind with nothing to tell it so.
    ready: tokio::sync::watch::Receiver<bool>,
}

/// How many times each of a session's agents has moved, for readers to wait on.
///
/// **Outlives the session actor, deliberately.** An idle session unloads, and a
/// reader waiting on one of these must not be cut off by that: the alternative
/// is a loop, because a disconnected browser reconnects, a reconnect loads the
/// session, and a loaded session goes idle again. The channel outliving the
/// actor is what breaks that cycle — a reader simply waits, and hears from the
/// session the next time something actually wakes it.
///
/// This is the same shape the old session-frame channel had, and for the same
/// reason; it is per-agent now because that is what a reader waits on.
///
/// A `watch` carries only the counter. It keeps the latest value and
/// overwrites, so a slow reader cannot fall behind it and there is nothing to
/// overflow — what actually happened is read from the agent's state.
///
/// Keyed by the *wire* agent id — `"main"`, or a subagent/step uuid — not by
/// `AgentKey`. The supervisor answers without loading the session, and telling
/// a `Sub` uuid from a `Step` uuid needs session state it deliberately does not
/// read. A uuid is one or the other and never both, so the wire id is already
/// unambiguous.
#[derive(Clone)]
pub struct Revisions {
    registry: std::sync::Arc<std::sync::Mutex<Registry>>,
    /// This session's feed, if the deployment has a bus to carry it. `None`
    /// only in tests that exercise the counters alone.
    feed: Option<crate::bus::Topic<AgentRevision>>,
}

/// This agent's channel within `registry`, created on first use.
///
/// Free-standing because the mirror task holds the registry and not a
/// [`Revisions`] — see [`Revisions::mirror`] for why it cannot.
fn sender_in(registry: &std::sync::Mutex<Registry>, id: &str) -> RevisionSender {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .agents
        .entry(id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::watch::Sender::new(0)))
        .clone()
}

impl Revisions {
    fn new(feed: Option<crate::bus::Topic<AgentRevision>>) -> Self {
        Self {
            registry: std::sync::Arc::default(),
            feed,
        }
    }

    /// This agent's channel, created on first use.
    #[must_use]
    pub fn for_agent(&self, id: &str) -> RevisionSender {
        sender_in(&self.registry, id)
    }

    /// This agent's channel, and every move of it announced to the rest of the
    /// deployment from now on.
    ///
    /// What the node **running** an agent uses, and the counterpart of
    /// [`Self::mirror`]. Separate from [`Self::for_agent`] rather than folded
    /// into it because the two ends must not both be taken on one node: a
    /// mirror that also forwarded would republish every value it received.
    ///
    /// Idempotent — a second call for the same agent joins the first.
    #[must_use]
    pub fn publishing(&self, id: &str) -> RevisionSender {
        let tx = self.for_agent(id);
        let Some(feed) = self.feed.clone() else {
            return tx;
        };
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if reg.forwarding.contains_key(id) {
            return tx;
        }
        let mut rx = tx.subscribe();
        let agent = id.to_string();
        // A `watch` keeps only the latest value, so this coalesces on its own:
        // a burst of deltas costs one frame rather than one per delta, and the
        // frame carries the value that burst arrived at.
        let task = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let revision = *rx.borrow_and_update();
                let frame = AgentRevision {
                    agent: agent.clone(),
                    revision,
                };
                if let Err(err) = feed.publish(&frame).await {
                    tracing::warn!(error = %err, agent = %agent, "could not announce an agent's revision");
                }
            }
        });
        reg.forwarding
            .insert(id.to_string(), Relay(task.abort_handle()));
        tx
    }

    /// Follow this session's agents from wherever they are running.
    ///
    /// What the node **answering** reads uses. The supervisor and the session
    /// are placed independently, so the counter a reader waits on is routinely
    /// moved in a process this one has no handle into; this is the copy it
    /// waits on instead.
    ///
    /// **Awaits the subscription being live**, rather than starting it in the
    /// background and returning. Redis discards anything published before a
    /// `SUBSCRIBE` registers, so returning early would leave the caller's first
    /// read racing its own subscription — and a bump lost that way is not
    /// corrected until the next one, which for an agent that has just finished
    /// may never come.
    ///
    /// Idempotent and cheap after the first call for a session.
    pub async fn mirror(&self) {
        if self.feed.is_none() {
            return;
        }
        let mut ready = self.start_mirror();
        // A subscription that could not be established drops its sender, which
        // ends this wait rather than hanging the reader — it polls on with a
        // counter only this node moves, which is what it did before any of this
        // existed.
        let live = async {
            while !*ready.borrow_and_update() {
                if ready.changed().await.is_err() {
                    break;
                }
            }
        };
        if tokio::time::timeout(MIRROR_SETUP, live).await.is_err() {
            tracing::warn!("gave up waiting for a session's revision feed");
        }
    }

    /// The mirror's readiness signal, starting it if this is the first ask.
    fn start_mirror(&self) -> tokio::sync::watch::Receiver<bool> {
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mirror) = &reg.mirror {
            return mirror.ready.clone();
        }
        let (tx, rx) = tokio::sync::watch::channel(false);
        let feed = self.feed.clone();
        // **Weak, or this leaks.** The task is owned by the registry it writes
        // into; holding it strongly would keep the registry alive forever, and
        // `SessionRevisions::release` dropping its last visible handle would
        // never actually drop it — so the abort below would never run.
        let registry = std::sync::Arc::downgrade(&self.registry);
        let task = tokio::spawn(async move {
            let Some(feed) = feed else { return };
            let mut reader = match feed.subscribe().await {
                Ok(reader) => reader,
                Err(err) => {
                    tracing::warn!(error = %err, "could not follow a session's revision feed");
                    return;
                }
            };
            let _ = tx.send(true);
            while let Some(frame) = reader.recv().await {
                let Some(registry) = registry.upgrade() else {
                    return;
                };
                // Set, never raise to a maximum. A session re-placed onto
                // another node starts a fresh registry and counts from zero, so
                // a smaller value is ordinary and must still wake a reader —
                // which only ever compares this number with the one it last
                // saw. `if_modified` keeps an unchanged value from waking one
                // for nothing.
                sender_in(&registry, &frame.agent).send_if_modified(|current| {
                    let moved = *current != frame.revision;
                    *current = frame.revision;
                    moved
                });
            }
        });
        reg.mirror = Some(Mirror {
            _relay: Relay(task.abort_handle()),
            ready: rx.clone(),
        });
        rx
    }

    /// Note that a reader just asked where an agent has got to.
    pub fn touch(&self) {
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.polled = Some(std::time::Instant::now());
    }

    /// Whether a reader is still interested, so the supervisor can drop the
    /// registry of a session nobody is watching.
    ///
    /// Recency, not a live receiver count. A reader holds a receiver only while
    /// its poll is waiting, and lets go for the moment it spends reading the log
    /// — so counting receivers would let an offload landing in that moment throw
    /// the registry away. The counter would restart at zero, the reader would
    /// see a change that did not happen, and its read would load the session
    /// again: the reload loop this registry exists to prevent, on a timer.
    ///
    /// A receiver count would also no longer mean what it says: the relay
    /// [`Self::publishing`] starts holds one for as long as the registry
    /// exists, so every session this node has ever run would read as watched
    /// and none would ever be released.
    #[must_use]
    pub fn watched(&self) -> bool {
        let reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        reg.polled.is_some_and(|at| at.elapsed() < WATCH_RETENTION)
    }
}

/// Every session's revision channels, and the account's own list counter.
///
/// **Node-local, and that is the constraint everything here is shaped by.** A
/// `watch` channel is a pointer into this process, so a reader served by
/// another host polls *that* host's copy of the number. What travels between
/// hosts is therefore always the number and never a handle to it.
///
/// Which is why the two counters here reach a reader by different routes, and
/// the difference is not arbitrary:
///
/// - the **list** counter is moved by the supervisor and read by the supervisor,
///   so a reader on any host reaches it with one `ask` and this copy is the
///   only copy that matters;
/// - an **agent** counter is moved by the *session* and read by the
///   *supervisor*, which are placed independently — so the session publishes
///   the value and whichever host answers a reader mirrors it into its own copy
///   here.
///
/// An earlier version of this comment said keeping the map on the account's
/// bundle solved that second case, because "a map on either actor would be
/// invisible to the other". The bundle is node-local too, so it was only ever
/// true while both actors happened to share a process.
#[derive(Default)]
pub struct SessionRevisions {
    /// This account, and the bus its nodes reach each other over — together,
    /// because a topic name needs both and neither is any use here alone.
    ///
    /// `None` leaves every counter node-local: the state of a test that only
    /// exercises the list. Every real deployment has a bus, a single-node one
    /// included, so this path stays the one the ordinary suite runs.
    feed: Option<(String, std::sync::Arc<dyn crate::bus::Bus>)>,
    sessions: std::sync::Mutex<std::collections::HashMap<String, Revisions>>,
    /// How many times this account's session list has changed — a status, a
    /// title, or a fork set. One counter for the whole list rather than one per
    /// session: a reader of the list re-reads the list, so knowing *that* it
    /// moved is all the counter has to carry.
    list: RevisionSender,
}

impl SessionRevisions {
    /// This account's counters, carried between nodes over `bus`.
    #[must_use]
    pub fn new(account: &str, bus: std::sync::Arc<dyn crate::bus::Bus>) -> Self {
        Self {
            feed: Some((account.to_string(), bus)),
            ..Self::default()
        }
    }

    /// The account's session-list counter, for a reader to wait on.
    #[must_use]
    pub fn list(&self) -> RevisionSender {
        self.list.clone()
    }

    /// Note that the session list changed.
    ///
    /// Absolute rather than a delta, which is what makes a missed observation
    /// harmless: whoever looks next sees the current value and re-reads the
    /// list, rather than needing every step in between.
    pub fn bump_list(&self) {
        self.list.send_modify(|v| *v += 1);
    }
    /// One session's channels, created on first use.
    #[must_use]
    pub fn of(&self, session: &str) -> Revisions {
        let feed = self.feed.as_ref().map(|(account, bus)| {
            crate::bus::topics::session_revisions(bus.clone(), account, session)
        });
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.to_string())
            .or_insert_with(|| Revisions::new(feed))
            .clone()
    }

    /// Drop a session's channels unless a reader is still interested.
    ///
    /// Called when a session unloads or is deleted. Keeping a watched one is
    /// the whole point: an unloaded session has nothing to say until something
    /// reloads it, and ending the stream would only make the client reconnect
    /// and reload it.
    pub fn release(&self, session: &str) {
        let mut map = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.get(session).is_none_or(|p| !p.watched()) {
            map.remove(session);
        }
    }
}

/// How long after a reader's last question its channels are kept. Comfortably
/// longer than one poll window, so an active reader always renews in time and a
/// departed one lapses shortly after.
const WATCH_RETENTION: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a reader waits for a session's feed before reading anyway. Long
/// enough for a round trip to the bus, short enough that an unreachable one
/// costs a reader a pause rather than its connection.
const MIRROR_SETUP: std::time::Duration = std::time::Duration::from_secs(5);

impl std::fmt::Debug for Revisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Revisions")
    }
}

/// Why a message could not be accepted. There is no "busy" here by design: a
/// turn in flight queues the message rather than rejecting it.
#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
pub enum UserMessageError {
    #[error("session not found")]
    NotFound,
    #[error("session is unrecoverable: {0}")]
    Unrecoverable(String),
    /// This session kind takes no messages — a workflow run works from its
    /// definition. Comes from `Orchestrator::accepts`, so the rule lives in one
    /// place rather than in a handler guard.
    #[error("{0}")]
    Rejected(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The list counter is absolute, not a delta.
    ///
    /// That is the property that makes a missed observation harmless: a reader
    /// that looks after two changes sees one number it can compare, re-reads
    /// the list, and is correct — it never needs the steps in between. Every
    /// decision about how this value reaches another host rests on it.
    #[test]
    fn the_list_revision_is_absolute_and_moves_on_every_change() {
        let revisions = SessionRevisions::default();
        let start = *revisions.list().borrow();
        revisions.bump_list();
        revisions.bump_list();
        assert_eq!(*revisions.list().borrow(), start + 2);
    }

    /// One counter for the whole list, not one per session.
    ///
    /// A reader of the list re-reads the list, so knowing *that* it moved is
    /// all this has to carry — and a counter per session would be a map to
    /// keep in step with the sessions themselves.
    #[test]
    fn every_session_shares_the_one_list_revision() {
        let revisions = SessionRevisions::default();
        let before = *revisions.list().borrow();
        revisions
            .of("session-a")
            .for_agent("main")
            .send_modify(|v| *v += 1);
        assert_eq!(
            *revisions.list().borrow(),
            before,
            "an agent moving is not the list changing"
        );
        revisions.bump_list();
        assert_eq!(*revisions.list().borrow(), before + 1);
    }

    /// One deployment's bus. Two `SessionRevisions` built on it are what two
    /// nodes are, since each node holds its own copy of this map.
    fn one_bus() -> std::sync::Arc<dyn crate::bus::Bus> {
        std::sync::Arc::new(crate::bus::MemoryBus::new())
    }

    /// Wait for one agent's counter to arrive at `want`, or give up.
    async fn settles_at(revisions: &Revisions, agent: &str, want: Revision) {
        let mut rx = revisions.for_agent(agent).subscribe();
        let arrives = async {
            while *rx.borrow_and_update() != want {
                rx.changed().await.expect("the counter outlives this wait");
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), arrives)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "expected {agent} to reach revision {want}, stuck at {}",
                    *rx.borrow()
                )
            });
    }

    /// The asymmetry this whole mechanism exists for: a **session** actor moves
    /// an agent's counter, and the **supervisor** answers reads of it, and the
    /// shard model places those two independently — so they are routinely on
    /// different nodes. Without this the reader long-polls forever against a
    /// number nobody on its node is moving.
    #[tokio::test]
    async fn an_agents_counter_reaches_a_node_that_only_mirrors_it() {
        let bus = one_bus();
        let runs_it = SessionRevisions::new("acct-1", bus.clone());
        let answers_for_it = SessionRevisions::new("acct-1", bus);

        // The node answering reads first, which is what starts it following.
        let mirror = answers_for_it.of("session-a");
        mirror.mirror().await;

        let local = runs_it.of("session-a").publishing("main");
        local.send_modify(|v| *v += 1);
        local.send_modify(|v| *v += 1);

        settles_at(&mirror, "main", 2).await;
    }

    /// The subscription is live by the time `mirror` returns, rather than being
    /// started in the background.
    ///
    /// A bus drops what is published while nobody is subscribed, so a mirror
    /// that returned first would leave its caller's read racing its own setup
    /// — and a value lost that way is not corrected until the next one, which
    /// for an agent that has just finished its last turn may never come. The
    /// reader would sit on a stale transcript with nothing to tell it so, which
    /// is the failure this whole change exists to remove.
    #[tokio::test]
    async fn a_value_published_the_instant_a_mirror_returns_is_not_lost() {
        let bus = one_bus();
        let answers_for_it = SessionRevisions::new("acct-1", bus.clone());
        let mirror = answers_for_it.of("session-a");
        mirror.mirror().await;

        // Straight onto the feed with nothing in between, which is what the
        // node running the session does at whatever moment it happens to move.
        crate::bus::topics::session_revisions(bus, "acct-1", "session-a")
            .publish(&AgentRevision {
                agent: "main".to_string(),
                revision: 7,
            })
            .await
            .unwrap();

        settles_at(&mirror, "main", 7).await;
    }

    /// A mirror takes the value it is told, including a smaller one.
    ///
    /// Not a lapse in monotonicity — a session re-placed onto another node
    /// starts a fresh registry, so its counter genuinely restarts at zero while
    /// a mirror still holds the old, larger value. Clamping to the maximum
    /// would leave that reader waiting forever. It costs nothing to allow,
    /// because a reader only ever compares this number with the last one it saw
    /// and re-reads when they differ; it never reads order into it.
    #[tokio::test]
    async fn a_mirror_follows_a_counter_that_restarts_lower() {
        let bus = one_bus();
        let runs_it = SessionRevisions::new("acct-1", bus.clone());
        let answers_for_it = SessionRevisions::new("acct-1", bus.clone());
        let mirror = answers_for_it.of("session-a");
        mirror.mirror().await;

        runs_it
            .of("session-a")
            .publishing("main")
            .send_modify(|v| *v = 42);
        settles_at(&mirror, "main", 42).await;

        // The same session, now running somewhere else: a registry that has
        // never seen it, counting up from nothing.
        let moved_to = SessionRevisions::new("acct-1", bus);
        drop(runs_it);
        moved_to
            .of("session-a")
            .publishing("main")
            .send_modify(|v| *v += 1);

        settles_at(&mirror, "main", 1).await;
    }

    /// One bus serves the whole deployment, so the account has to be in the
    /// topic name. Without it a reader of one account's session would be woken
    /// by — and would then read — another account's.
    #[tokio::test]
    async fn one_accounts_counter_never_reaches_anothers_mirror() {
        let bus: std::sync::Arc<dyn crate::bus::Bus> =
            std::sync::Arc::new(crate::bus::MemoryBus::new());
        let (mine, theirs) = (
            SessionRevisions::new("acct-1", bus.clone()),
            SessionRevisions::new("acct-2", bus.clone()),
        );
        let eavesdropper = theirs.of("session-a");
        eavesdropper.mirror().await;
        // A mirror of my own, so the assertion below waits on something that
        // has definitely happened rather than on a bare sleep.
        let control = SessionRevisions::new("acct-1", bus);
        let control = control.of("session-a");
        control.mirror().await;

        mine.of("session-a")
            .publishing("main")
            .send_modify(|v| *v += 1);

        settles_at(&control, "main", 1).await;
        assert_eq!(
            *eavesdropper.for_agent("main").borrow(),
            0,
            "another account's session must not be readable by name alone"
        );
    }
}
