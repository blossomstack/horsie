//! What an agent is equipped with.
//!
//! Everything an agent can do is a capability — the sandbox toolbox, the
//! memory and MCP layers, `ask_user`, `set_session_title`, `spawn_agent`,
//! `submit_result`. One mechanism rather than two, which is what lets a
//! workflow whose first step is interactive and whose second is not equip
//! exactly the right tools without a second way of saying so.
//!
//! A capability is a **value**, held in its runner's slice and journaled with
//! it, so what an agent could do survives a reload and cannot drift from what
//! the log says. Instances belong to the runner — a workflow runner holds one
//! [`sub_agent::SubAgentCapability`] whose outstanding children outlive any
//! single step — while *equipment* is computed per agent, by folding a subset
//! over its [`AgentSpec`].
//!
//! # Dispatch
//!
//! [`Capability::handle`] returns `Option`: `None` means "not mine". One
//! method rather than a `supports` predicate beside a handler, because a
//! capability that answered yes and then could not cope, and a pair edited out
//! of step, are states that cannot be written this way.
//!
//! Tool calls and commands are offered around by [`offer`]; a child's outcome
//! and an arriving answer are addressed to their owner instead, because
//! exactly one capability created that child or recorded that ask. Offering
//! those around would let two capabilities plausibly claim the same outcome,
//! which is the ambiguity most worth designing out.
//!
//! Order is therefore the conflict resolution for tool calls, and it is a
//! written property of assembly rather than an accident of construction: the
//! open-namespace capabilities — [`runtime::RuntimeCapability`] above all —
//! sort last, because they answer for a namespace nobody can enumerate.

pub mod ask_user;
pub mod control_plane;
pub mod fork;
pub mod mcp;
pub mod memory;
pub mod runtime;
pub mod step_result;
pub mod sub_agent;
pub mod title;
pub mod workflow;

use super::action::{Action, AgentSpec};
use super::message::{Caller, Message};
use serde::{Deserialize, Serialize};

/// What a capability decided: events for its own slice, actions for the
/// session. The pair every decision in this module returns.
pub type Decision = (Vec<CapEvent>, Vec<Action>);

/// One capability's behaviour.
pub trait Handler {
    /// Equip the agent: toolbox layer, prompt section.
    ///
    /// Called per agent, so a capability may equip one of its runner's agents
    /// and not another — a workflow step that declares itself interactive gets
    /// `ask_user`, and the next step does not.
    fn setup(&self, spec: &mut AgentSpec);

    /// `None` means "not mine".
    ///
    /// `&Message` rather than by value because the same message is offered to
    /// each capability until one takes it; the taker clones what it keeps.
    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision>;

    /// Fold one of my own events. Pure: no clock, no randomness, no id
    /// generation — those belong in `handle`, which is a decision rather than
    /// a replay.
    fn apply(&mut self, event: &CapEvent);
}

/// The capabilities a runner can hold.
///
/// A closed enum rather than `Box<dyn Handler>` so the list serialises into
/// the runner's slice, and so a new capability is a compile error in the two
/// places that must know about it rather than a silent gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Capability {
    Runtime(runtime::RuntimeCapability),
    Mcp(mcp::McpCapability),
    Memory(memory::MemoryCapability),
    ControlPlane(control_plane::ControlPlaneCapability),
    AskUser(ask_user::AskUserCapability),
    Title(title::TitleCapability),
    SubAgent(sub_agent::SubAgentCapability),
    Workflow(workflow::WorkflowCapability),
    Fork(fork::ForkCapability),
    StepResult(step_result::StepResultCapability),
}

/// One capability's event, tagged with which capability owns it.
///
/// Typed rather than an opaque blob: the journal stays readable, and a shape
/// change fails to compile where it should.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapEvent {
    AskUser(ask_user::Event),
    Title(title::Event),
    SubAgent(sub_agent::Event),
    Workflow(workflow::Event),
    Fork(fork::Event),
    StepResult(step_result::Event),
}

macro_rules! dispatch {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            Capability::Runtime(c) => c.$method($($arg),*),
            Capability::Mcp(c) => c.$method($($arg),*),
            Capability::Memory(c) => c.$method($($arg),*),
            Capability::ControlPlane(c) => c.$method($($arg),*),
            Capability::AskUser(c) => c.$method($($arg),*),
            Capability::Title(c) => c.$method($($arg),*),
            Capability::SubAgent(c) => c.$method($($arg),*),
            Capability::Workflow(c) => c.$method($($arg),*),
            Capability::Fork(c) => c.$method($($arg),*),
            Capability::StepResult(c) => c.$method($($arg),*),
        }
    };
}

impl Handler for Capability {
    fn setup(&self, spec: &mut AgentSpec) {
        dispatch!(self, setup, spec);
    }

    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        dispatch!(self, handle, caller, msg)
    }

    fn apply(&mut self, event: &CapEvent) {
        dispatch!(self, apply, event);
    }
}

/// Equip an agent by folding every capability over a fresh spec.
///
/// This replaces the four-arm match that used to decide an agent's toolbox
/// layers from its kind, and the second four-arm match that decided its prompt
/// suffix. One fold, one source, and no way to advertise a tool whose result
/// nothing can process.
#[must_use]
pub fn equip(caps: &[Capability], settings: crate::sessions::spec::AgentSettings) -> AgentSpec {
    let mut spec = AgentSpec {
        settings: Some(settings),
        ..AgentSpec::default()
    };
    for cap in caps {
        cap.setup(&mut spec);
    }
    spec
}

/// Offer a message to each capability until one takes it.
///
/// `None` from all of them is an error at the one place this is called, never
/// a silent drop: it replaces an exhaustive-match compile error, which is a
/// real downgrade in safety, so what is left has to be loud.
#[must_use]
pub fn offer(caps: &[Capability], caller: Caller, msg: &Message) -> Option<Decision> {
    caps.iter().find_map(|c| c.handle(caller, msg))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod testing {
    use super::*;
    use crate::sessions::runners::ids::AgentId;
    use crate::sessions::spec::AgentSettings;

    /// The shared empty settings, re-exported so a capability test does not
    /// have to know where it lives.
    #[must_use]
    pub(crate) fn settings() -> AgentSettings {
        crate::sessions::runners::empty_settings()
    }

    #[must_use]
    pub(crate) fn caller() -> Caller {
        Caller {
            agent: AgentId::new_v4(),
            depth: 0,
            active_agents: 0,
        }
    }

    #[must_use]
    pub(crate) fn tool(name: &str, input: serde_json::Value) -> Message {
        Message::Tool(crate::sessions::runners::message::ToolCall {
            id: "t".into(),
            name: name.into(),
            input,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::testing::*;
    use super::*;

    /// A fixed-name capability wins over the open-namespace one behind it.
    /// Order is the conflict resolution for tool calls, so it is a property of
    /// assembly and gets a test rather than a comment.
    #[test]
    fn a_fixed_name_capability_beats_the_fallback_behind_it() {
        let caps = vec![
            Capability::Title(title::TitleCapability::default()),
            Capability::Runtime(runtime::RuntimeCapability),
        ];
        let (events, _) = offer(
            &caps,
            caller(),
            &tool("set_session_title", serde_json::json!({"title": "x"})),
        )
        .expect("someone takes it");
        assert!(matches!(events.first(), Some(CapEvent::Title(_))));
    }

    /// And the same set in the wrong order routes it to the fallback, which is
    /// exactly the silent shadowing the written order exists to prevent.
    #[test]
    fn the_wrong_order_lets_the_fallback_shadow_a_named_tool() {
        let caps = vec![
            Capability::Runtime(runtime::RuntimeCapability),
            Capability::Title(title::TitleCapability::default()),
        ];
        let (events, _) = offer(
            &caps,
            caller(),
            &tool("set_session_title", serde_json::json!({"title": "x"})),
        )
        .expect("the fallback takes it");
        assert!(
            events.is_empty(),
            "the runtime capability journals nothing, so the title was lost"
        );
    }

    /// A call nobody claims is `None` at the one place the scan lives.
    #[test]
    fn a_call_nobody_claims_is_none() {
        let caps = vec![Capability::Title(title::TitleCapability::default())];
        assert!(offer(&caps, caller(), &tool("nope", serde_json::json!({}))).is_none());
    }

    /// Equipping folds every capability over one spec, so what an agent can do
    /// is the sum of its runner's capabilities and nothing else.
    #[test]
    fn equipping_folds_every_capability() {
        let caps = vec![
            Capability::Title(title::TitleCapability::default()),
            Capability::Runtime(runtime::RuntimeCapability),
        ];
        let spec = equip(&caps, settings());
        assert!(spec.has(&crate::sessions::runners::action::ToolLayer::SessionTitle));
        assert!(spec.has(&crate::sessions::runners::action::ToolLayer::Runtime));
    }
}
