//! `spawn_agent` and `subagent_status`: handing work to a worker, and being
//! told how it went.
//!
//! Held by every runner whose agents may delegate — a conversation, a workflow
//! run whose outstanding children outlive any one step, and a subagent runner,
//! which is what makes a subagent of a subagent ordinary rather than a case.
//! One implementation serves all three because an agent may not conclude while
//! it has outstanding children, so every parent does the identical thing on a
//! report: deliver it to the agent that asked.
//!
//! [`SubAgentCapability::outstanding`] is the single fact behind both questions
//! anyone asks about delegated work — is a report still owed, and who is owed
//! it. A `notified` flag beside it could disagree with it; there is nothing
//! here to disagree. It is also the re-drive point: delivery tells the parent
//! before the acknowledgement persists, so a crash in that window replays as a
//! report still outstanding and it is delivered again.
//!
//! # What a spawn is refused for
//!
//! Two budgets, both read off [`Caller`] and neither of them a fact this
//! capability could know on its own: how deep the asking runner already sits,
//! and how much of the session is already running. A refusal is a
//! [`Decision::reply`] and journals nothing — the model is told no, and no
//! trace of a child that never existed reaches the log.
//!
//! It is still *claimed*. Declining would hand the call to the next capability,
//! and the last one is the open-namespace runtime, which answers to every name:
//! the model would be answered by the sandbox and never learn it had hit a
//! budget.
//!
//! The old session actor made a third refusal here — `"caller is not a known
//! agent"`, when the spawning agent had no node in the forest. It is not one of
//! these, and deliberately so: a [`Caller`] is built by the session from the
//! agent that called, resolved against the agent-to-runner map, so a call this
//! capability is offered has already been attributed. The refusal belongs at
//! that lookup, which is the only place that can still fail.

use super::{CapEvent, CapSlice, Capability, Decision, SetupError, or_empty};
use crate::sessions::runners::action::{Action, RunnerArgs};
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::{AgentSpec, Loading};
use crate::sessions::runners::message::{
    Caller, ChildMsg, ChildOutcome, Message, SubAgentOutcome, ToolCall,
};
use crate::sessions::session_actor::AgentKey;
use crate::sessions::spawn_tool::SubAgentToolbox;
use crate::sessions::spec::AgentSettings;
use crate::sessions::subagents::{MAX_SUBAGENT_DEPTH, SubAgentParent};
use horsie_models::agent::SubAgentResultPart;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The tool that delegates.
pub const SPAWN_TOOL: &str = "spawn_agent";
/// The tool that reads back what is still running.
pub const STATUS_TOOL: &str = "subagent_status";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentCapability {
    /// Fixed when the owning runner built this: what children inherit. A
    /// child's equipment is decided at the moment its parent was equipped, not
    /// at the moment it is spawned, so a settings change mid-session cannot
    /// give two siblings different tools.
    pub child_settings: AgentSettings,
    /// Which child, and which of my agents asked for it.
    pub outstanding: BTreeMap<RunnerId, AgentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Started { child: RunnerId, from: AgentId },
    Reported { child: RunnerId },
}

/// The tool's arguments. Deserialised here so the schema and this type are one
/// declaration rather than two that can drift.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub label: String,
    pub task: String,
    /// A plugin-declared agent type, or `None` for a worker that inherits its
    /// parent's instructions and tools.
    pub agent_type: Option<String>,
}

impl SubAgentCapability {
    #[must_use]
    pub fn new(child_settings: AgentSettings) -> Self {
        Self {
            child_settings,
            outstanding: BTreeMap::new(),
        }
    }

    fn on_tool(&self, caller: Caller, t: &ToolCall) -> Option<Decision> {
        match t.name.as_str() {
            SPAWN_TOOL => {
                let req: Request = match super::parse(&t.name, &t.input) {
                    Ok(req) => req,
                    Err(refusal) => return Some(refusal),
                };
                // Claimed, not declined: see the note on `refuse`.
                if let Some(said) = self.refuse(caller) {
                    return Some(Decision::reply(said));
                }
                // Both ids are minted here rather than in `apply`: a decision
                // may be non-deterministic, a fold may not. Replay must land
                // the ids the log recorded, so the event and the action name
                // the same child and neither invents one.
                //
                // The worker's agent id is minted *with* its runner id, and not
                // when the worker's agent starts, because `spawn_agent`'s reply
                // names it and that reply fires as soon as the create is
                // durable. Two ids and not one: a runner and an agent are
                // separate spaces, and a workflow runner owns many agents, so
                // an equality would hold here and be false there.
                let child = RunnerId::new_v4();
                let agent = AgentId::new_v4();
                Some(Decision {
                    events: vec![CapEvent::SubAgent(Event::Started {
                        child,
                        from: caller.agent,
                    })],
                    actions: vec![Action::CreateChild {
                        id: child,
                        kind: RunnerKind::SubAgent,
                        args: RunnerArgs::SubAgent {
                            agent,
                            label: req.label,
                            task: req.task,
                            agent_type: req.agent_type,
                            settings: Box::new(self.child_settings.clone()),
                        },
                        parent: caller.agent,
                    }],
                })
            }
            STATUS_TOOL => Some(Decision::reply(self.render_status())),
            _ => None,
        }
    }

    /// Why this spawn cannot happen, in words the model can act on.
    ///
    /// Both numbers come off the [`Caller`] the session built, because neither
    /// is knowable from this slice: `outstanding` holds the children *this*
    /// runner is waiting on, and the budgets are properties of the session
    /// around it.
    ///
    /// The cap is read from `child_settings`, which is the *owning* agent's
    /// settings — [`crate::sessions::runners::assemble`] builds this capability
    /// from them, and a child inherits them unchanged. So a workflow step's
    /// spawns are counted against the step's preset, exactly as the session
    /// actor counted them against the caller's, and the same number that makes
    /// [`Capability::setup`] advertise nothing at zero is the one that refuses
    /// here.
    fn refuse(&self, caller: Caller) -> Option<String> {
        // `depth` is the *asking* runner's, so the first worker of a
        // conversation is spawned from depth 0 and lands at 1 — which is why
        // the bound is `>=` and not `>`.
        if caller.depth >= MAX_SUBAGENT_DEPTH {
            return Some(format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached"));
        }
        let max = self.child_settings.max_subagents();
        if caller.active_agents >= max {
            return Some(format!("{max} subagents already active"));
        }
        None
    }

    fn on_child(&self, m: &ChildMsg) -> Option<Decision> {
        match m {
            ChildMsg::Outcome {
                child,
                outcome: ChildOutcome::SubAgent(o),
            } => {
                // Not one of mine: fall through as `None` rather than deliver
                // somebody else's report, so "addressed by owner" is enforced
                // by the same return type as "not my tool".
                let to = *self.outstanding.get(child)?;
                Some(self.deliver(*child, to, part(*child, o)))
            }
            // A run's outcome is the workflow capability's even when both are
            // held by the same runner. The outcome's kind and the owning
            // capability have to agree, and `None` is how they do.
            ChildMsg::Outcome {
                outcome: ChildOutcome::Workflow(_),
                ..
            } => None,
            // A child that never started still owes its asker an answer: the
            // agent is sitting on a spawn it was told succeeded.
            ChildMsg::Failed { child, error } => {
                let to = *self.outstanding.get(child)?;
                Some(self.deliver(
                    *child,
                    to,
                    failed_part(*child, child.to_string(), error.clone()),
                ))
            }
            // A worker is runnable the moment it is created; only a fork has a
            // seed that can land later.
            ChildMsg::Ready { .. } => None,
        }
    }

    fn deliver(&self, child: RunnerId, to: AgentId, part: SubAgentResultPart) -> Decision {
        let reported = CapEvent::SubAgent(Event::Reported { child });
        Decision::record(vec![reported]).then(Action::Deliver {
            to,
            from: child,
            part: Box::new(part),
        })
    }

    /// Only what is still running: a child that reported has been delivered
    /// into the asking agent's own transcript, so listing it again would show
    /// the model its own history back.
    fn render_status(&self) -> String {
        if self.outstanding.is_empty() {
            return "No subagents are running.".to_string();
        }
        let mut text = format!("{} subagent(s) running:", self.outstanding.len());
        for (child, from) in &self.outstanding {
            text.push_str(&format!("\n- {child} (spawned by {from})"));
        }
        text
    }
}

/// A finished worker's report, in the shape the parent's inbox takes.
///
/// The timestamps are zero because this capability holds neither: they live on
/// the child's `RunnerRecord`. A client shows no duration rather than one that
/// never happened.
fn part(child: RunnerId, outcome: &SubAgentOutcome) -> SubAgentResultPart {
    match outcome {
        SubAgentOutcome::Completed { label, report } => SubAgentResultPart {
            subagent_id: child.to_string(),
            label: label.clone(),
            status: "completed".to_string(),
            text: report.clone(),
            spawned_at_ms: 0,
            ended_at_ms: 0,
        },
        SubAgentOutcome::Failed { label, error } => {
            failed_part(child, label.clone(), error.clone())
        }
    }
}

fn failed_part(child: RunnerId, label: String, error: String) -> SubAgentResultPart {
    SubAgentResultPart {
        subagent_id: child.to_string(),
        label,
        status: "failed".to_string(),
        text: error,
        spawned_at_ms: 0,
        ended_at_ms: 0,
    }
}

#[async_trait::async_trait]
impl Capability for SubAgentCapability {
    fn name(&self) -> &'static str {
        "sub_agent"
    }

    /// Equips `spawn_agent`, with the catalogue read at compose time.
    ///
    /// The catalogue only exists after the runtime's workspace scan, and this
    /// capability sorts *before* the runtime — it has to, or the
    /// open-namespace sandbox layer would swallow the `spawn_agent` name. So
    /// the layer reads [`crate::sessions::runners::loading::AgentFacts`] when
    /// it is composed, which is after every `setup` has run, rather than
    /// capturing a catalogue that does not exist yet.
    async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        // A zero cap advertises nothing. A tool the model can only ever be
        // refused by is worse than no tool: it spends prompt on a capability
        // that does not exist and invites a retry loop against a fixed number.
        if self.child_settings.max_subagents() == 0 {
            return Ok(());
        }
        let session = loading.session.clone();
        // Who is being equipped, in the runners' own id space. `parent` below
        // cannot say it — `SubAgentParent` collapses a main agent, a step and
        // a fork into one variant — so the tool carries both.
        let agent = loading.agent;
        // Where this agent's children hang. A step and a fork each root their
        // own tree, for the same reason: nothing is waiting on them for a
        // report, so their spawns are that tree's `Main`.
        let parent = match loading.key {
            AgentKey::Sub(id) => SubAgentParent::SubAgent(id),
            AgentKey::Main | AgentKey::Step(_) | AgentKey::Fork(_) => SubAgentParent::Main,
        };
        spec.wrap(move |inner, facts| {
            Arc::new(SubAgentToolbox::new(
                or_empty(inner),
                session,
                parent,
                agent,
                facts
                    .shared
                    .as_ref()
                    .map(|s| Arc::clone(&s.agents))
                    .unwrap_or_default(),
            ))
        });
        spec.say(
            "sub_agent",
            "Delegate independent work with `spawn_agent`. A subagent \
             does not see this conversation, so its task must be \
             self-contained; its final message is its report, and that \
             report is delivered to you automatically when it finishes. \
             Carry on with other work meanwhile, and use \
             `subagent_status` only when asked for progress or when a \
             result seems lost — never as a poll.",
        );
        Ok(())
    }

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        match msg {
            Message::Tool(t) => self.on_tool(caller, t),
            Message::Child(m) => self.on_child(m),
            Message::Command(_) => None,
        }
    }

    fn apply(&mut self, event: &CapEvent) {
        let CapEvent::SubAgent(e) = event else { return };
        match e {
            Event::Started { child, from } => {
                self.outstanding.insert(*child, *from);
            }
            Event::Reported { child } => {
                self.outstanding.remove(child);
            }
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::SubAgent(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    fn cap() -> SubAgentCapability {
        SubAgentCapability::new(settings())
    }

    fn spawn_call() -> Message {
        tool(SPAWN_TOOL, serde_json::json!({"label": "l", "task": "t"}))
    }

    /// What the model was told, having checked that it was told rather than
    /// obeyed. A refusal is not a fact about the session, so an event here
    /// would put a child that never existed in the log.
    fn refusal(d: &Decision) -> String {
        assert!(
            d.events.is_empty(),
            "a refusal is not a fact about the session"
        );
        let [Action::Reply { text }] = d.actions.as_slice() else {
            panic!("expected one reply, got {:?}", d.actions);
        };
        text.clone()
    }

    fn spawn(c: &mut SubAgentCapability, caller: Caller) -> RunnerId {
        let d = c
            .handle(
                caller,
                &tool(SPAWN_TOOL, serde_json::json!({"label": "l", "task": "t"})),
            )
            .expect("mine");
        c.apply(&d.events[0]);
        let Action::CreateChild { id, .. } = &d.actions[0] else {
            panic!("expected a create, got {:?}", d.actions[0]);
        };
        *id
    }

    /// The event and the action must name the same child. If they ever differ,
    /// the log records a child nothing created and the agent waits for ever.
    #[test]
    fn a_spawn_journals_and_creates_the_same_child() {
        let c = cap();
        let caller = caller();
        let d = c
            .handle(
                caller,
                &tool(
                    SPAWN_TOOL,
                    serde_json::json!({"label": "read the flake", "task": "look"}),
                ),
            )
            .expect("mine");
        let CapEvent::SubAgent(Event::Started { child, from }) = &d.events[0] else {
            panic!("expected a start, got {:?}", d.events[0]);
        };
        assert_eq!(*from, caller.agent);
        let Action::CreateChild {
            id,
            kind,
            args,
            parent,
        } = &d.actions[0]
        else {
            panic!("expected a create, got {:?}", d.actions[0]);
        };
        assert_eq!(id, child);
        assert_eq!(*kind, RunnerKind::SubAgent);
        assert_eq!(*parent, caller.agent);
        let RunnerArgs::SubAgent { agent, label, .. } = args else {
            panic!("expected subagent args, got {args:?}");
        };
        // The worker's agent is decided here, with its runner, because
        // `spawn_agent`'s reply names it — and it is its *own* id, not the
        // runner's. Two spaces on purpose: a workflow runner owns many agents,
        // so an equality that held for a worker would be false for a run.
        assert_ne!(agent.as_uuid(), child.as_uuid());
        assert_eq!(label, "read the flake");
    }

    /// The bound on nesting. Without it a worker that spawns a worker is a
    /// machine that runs until something else stops it — which is what
    /// `MAX_SUBAGENT_DEPTH` exists to be, and the number is the constant's, not
    /// one written twice.
    #[test]
    fn a_spawn_at_the_depth_limit_is_refused() {
        let c = cap();
        // The last depth that may still delegate, and the first that may not.
        let ok = c
            .handle(
                Caller {
                    depth: MAX_SUBAGENT_DEPTH - 1,
                    ..caller()
                },
                &spawn_call(),
            )
            .expect("mine");
        assert!(matches!(ok.actions[0], Action::CreateChild { .. }));

        let d = c
            .handle(
                Caller {
                    depth: MAX_SUBAGENT_DEPTH,
                    ..caller()
                },
                &spawn_call(),
            )
            .expect("mine, refused or not");
        assert_eq!(refusal(&d), "max subagent depth 4 reached");
    }

    /// The concurrency cap, and where its number comes from: the settings this
    /// capability was built with — the owning agent's, which a child inherits —
    /// so a workflow step's spawns are counted against the step's preset rather
    /// than a session-wide value nothing in a run owns.
    #[test]
    fn a_spawn_over_the_concurrency_cap_is_refused() {
        // The session-wide default, spelled by the caller's count reaching it.
        let c = cap();
        let d = c
            .handle(
                Caller {
                    active_agents: 8,
                    ..caller()
                },
                &spawn_call(),
            )
            .expect("mine, refused or not");
        assert_eq!(refusal(&d), "8 subagents already active");

        // And a step whose preset allows one: its budget is spent by one.
        let mut s = settings();
        s.max_concurrent_subagents = Some(1);
        let c = SubAgentCapability::new(s);
        let ok = c
            .handle(caller(), &spawn_call())
            .expect("nothing is running yet");
        assert!(matches!(ok.actions[0], Action::CreateChild { .. }));
        let d = c
            .handle(
                Caller {
                    active_agents: 1,
                    ..caller()
                },
                &spawn_call(),
            )
            .expect("mine, refused or not");
        assert_eq!(refusal(&d), "1 subagents already active");
    }

    /// **A refused spawn must still be claimed.** Declining hands the call to
    /// the next capability, and the last one is the open-namespace runtime that
    /// answers to every name — so the model would be answered by the sandbox
    /// and never learn it had hit a budget.
    #[test]
    fn a_refused_spawn_is_claimed_rather_than_left_to_the_sandbox() {
        let caps = crate::sessions::runners::capabilities::Capabilities::new(vec![
            Box::new(cap()),
            Box::new(crate::sessions::runners::capabilities::runtime::RuntimeCapability::default()),
        ]);
        for over_budget in [
            Caller {
                depth: MAX_SUBAGENT_DEPTH,
                ..caller()
            },
            Caller {
                active_agents: 8,
                ..caller()
            },
        ] {
            let taken = caps
                .iter()
                .find_map(|c| c.handle(over_budget, &spawn_call()).map(|d| (c.name(), d)));
            let Some(("sub_agent", d)) = taken else {
                panic!("the sandbox layer swallowed the spawn: {taken:?}");
            };
            assert!(!refusal(&d).is_empty());
        }
    }

    /// `outstanding` says both "a report is owed" and "to whom". A `Started`
    /// that did not record the asker would leave a finished worker with
    /// nowhere to deliver.
    #[test]
    fn started_records_the_child_and_reported_clears_it() {
        let mut c = cap();
        let caller = caller();
        let child = spawn(&mut c, caller);
        assert_eq!(c.outstanding.get(&child), Some(&caller.agent));

        c.apply(&CapEvent::SubAgent(Event::Reported { child }));
        assert!(c.outstanding.is_empty());
    }

    /// A report goes to the agent that asked, not to whoever happens to be
    /// running — which is the whole reason `outstanding` maps to an `AgentId`.
    #[test]
    fn a_completed_report_is_delivered_to_the_agent_that_asked() {
        let mut c = cap();
        let asker = caller();
        let child = spawn(&mut c, asker);

        // Delivered during some other agent's turn on purpose: the address
        // comes from what the log recorded, never from who is speaking now.
        let d = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "found it".into(),
                    }),
                }),
            )
            .expect("mine");
        assert!(matches!(
            d.events[0],
            CapEvent::SubAgent(Event::Reported { .. })
        ));
        let Action::Deliver { to, from, part } = &d.actions[0] else {
            panic!("expected a delivery, got {:?}", d.actions[0]);
        };
        assert_eq!(*to, asker.agent);
        assert_eq!(*from, child);
        assert_eq!(part.status, "completed");
        assert_eq!(part.text, "found it");
        assert_eq!(part.subagent_id, child.to_string());
        assert_eq!(part.label, "l");
    }

    /// A failure is a report too. An agent blocked on a worker that died and
    /// was never told would wait for ever.
    #[test]
    fn a_failed_outcome_is_delivered_as_a_failed_part() {
        let mut c = cap();
        let child = spawn(&mut c, caller());
        let d = c
            .handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child,
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Failed {
                        label: "l".into(),
                        error: "it broke".into(),
                    }),
                }),
            )
            .expect("mine");
        let Action::Deliver { part, .. } = &d.actions[0] else {
            panic!("expected a delivery, got {:?}", d.actions[0]);
        };
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "it broke");
    }

    /// A child that never started takes the same delivery path: the asker is
    /// holding an id it was told was real.
    #[test]
    fn a_child_that_never_started_is_reported_as_failed() {
        let mut c = cap();
        let caller = caller();
        let child = spawn(&mut c, caller);
        let d = c
            .handle(
                caller,
                &Message::Child(ChildMsg::Failed {
                    child,
                    error: "the create failed".into(),
                }),
            )
            .expect("mine");
        assert!(matches!(
            d.events[0],
            CapEvent::SubAgent(Event::Reported { .. })
        ));
        let Action::Deliver { to, part, .. } = &d.actions[0] else {
            panic!("expected a delivery, got {:?}", d.actions[0]);
        };
        assert_eq!(*to, caller.agent);
        assert_eq!(part.status, "failed");
        assert_eq!(part.text, "the create failed");
    }

    /// A child this capability did not create is not its business. Without the
    /// `?`, a sibling runner's report would be delivered to an agent that
    /// never asked for it.
    #[test]
    fn an_outcome_for_a_child_i_did_not_create_is_not_mine() {
        let c = cap();
        assert!(
            c.handle(
                caller(),
                &Message::Child(ChildMsg::Outcome {
                    child: RunnerId::new_v4(),
                    outcome: ChildOutcome::SubAgent(SubAgentOutcome::Completed {
                        label: "l".into(),
                        report: "r".into(),
                    }),
                }),
            )
            .is_none()
        );
    }

    /// A status read journals nothing: it is a read, and an event for it would
    /// grow the log every time the model looked.
    #[test]
    fn status_lists_the_outstanding_children_and_journals_nothing() {
        let mut c = cap();
        let caller = caller();
        let child = spawn(&mut c, caller);
        let d = c
            .handle(caller, &tool(STATUS_TOOL, serde_json::json!({})))
            .expect("mine");
        assert!(d.events.is_empty());
        let Action::Reply { text } = &d.actions[0] else {
            panic!("expected a reply, got {:?}", d.actions[0]);
        };
        assert!(text.contains(&child.to_string()));

        c.apply(&CapEvent::SubAgent(Event::Reported { child }));
        let d = c
            .handle(caller, &tool(STATUS_TOOL, serde_json::json!({})))
            .expect("mine");
        let Action::Reply { text } = &d.actions[0] else {
            panic!("expected a reply, got {:?}", d.actions[0]);
        };
        assert!(!text.contains(&child.to_string()));
    }

    /// Both tools, and the paragraph that says how to use them.
    #[tokio::test]
    async fn setup_equips_the_spawn_tools() {
        let mut spec = spec();
        cap()
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.prompt.iter().any(|s| s.key == "sub_agent"));
        let names = equipped(spec);
        assert!(names.contains(&SPAWN_TOOL.to_string()));
        assert!(names.contains(&STATUS_TOOL.to_string()));
    }

    /// The catalogue this capability offers is the one the runtime's scan
    /// found — read when the layer is composed, not when it is pushed, because
    /// `spawn_agent` has to be claimed before the runtime and the scan has not
    /// happened by then. If this regresses, every typed agent silently
    /// disappears from `spawn_agent`'s description.
    #[tokio::test]
    async fn the_catalogue_is_read_after_the_runtimes_scan() {
        let mut spec = spec();
        cap()
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing fatal");
        // Written after the layer was pushed, exactly as the runtime does.
        let reviewer = crate::agent_loop::CatalogAgent {
            plugin: "b".into(),
            def: horsie_support::plugin::agents::PluginAgentDef {
                name: "reviewer".into(),
                description: "reads a diff".into(),
                model: None,
                tools: vec![],
                prompt: "you review".into(),
            },
        };
        spec.facts.shared = Some(Arc::new(crate::agent_loop::SharedContext {
            skills: Arc::default(),
            agents: Arc::new([reviewer].into_iter().collect()),
            root: None,
        }));
        let spawn = spec
            .toolbox()
            .expect("a layer was pushed")
            .specs()
            .into_iter()
            .find(|s| s.name == SPAWN_TOOL)
            .expect("spawn_agent is advertised");
        // The catalogue is rendered into the description — a bare list of names
        // says nothing about when to pick one — so that is where it is read
        // back from.
        assert!(
            spawn.description.contains("reviewer") && spawn.description.contains("reads a diff"),
            "the catalogue the scan found did not reach the tool: {}",
            spawn.description
        );
    }

    /// A zero cap advertises no tool at all, so the model never meets one that
    /// can only refuse.
    #[tokio::test]
    async fn a_zero_cap_advertises_nothing() {
        let mut s = settings();
        s.max_concurrent_subagents = Some(0);
        let mut spec = spec();
        SubAgentCapability::new(s)
            .setup(&loading(), &mut spec)
            .await
            .expect("nothing fatal");
        assert!(spec.prompt.is_empty());
        assert!(spec.toolbox().is_none());
    }

    /// Everything else falls through, so the offer reaches the capability that
    /// does own it.
    #[test]
    fn another_message_is_not_mine() {
        let c = cap();
        assert!(
            c.handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
        assert!(
            c.handle(
                caller(),
                &Message::Command(crate::sessions::runners::message::Command {
                    name: "fork".into(),
                    args: String::new(),
                })
            )
            .is_none()
        );
    }
}
