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
//! # Two sides
//!
//! A capability is driven from two places, and which one drives which method is
//! the single fact worth holding on to:
//!
//! - The **agent's own task** walks [`Capability::setup`] in order when the
//!   agent is loaded, and [`Capability::teardown`] when it is unloaded. Both
//!   are async, because acquiring a sandbox or connecting an MCP server is slow
//!   and must not block the session mailbox.
//! - The **session actor** offers [`Capability::handle`] every message the
//!   agent produces, and folds what comes back through [`Capability::apply`].
//!   Both are sync and pure — they decide, and the session performs.
//!
//! So the list a `setup` runs against is built fresh for the agent being
//! started, and the folded copy stays with the session — the same
//! [`super::assemble`] call, doing two different jobs.
//!
//! Not quite the same *set*, in one case. A workflow adds the step's own
//! [`step_result::StepResultCapability`] to the copy and not to the folded
//! list, because what a step promises to return is declared per step and there
//! is nothing about it to carry between them. It is added with
//! [`Capabilities::push_front`], since a capability with a fixed tool name must
//! sort ahead of the open-namespace ones.
//!
//! # Dispatch
//!
//! [`Capability::handle`] returns `Option`: `None` means "not mine". One
//! method rather than a `supports` predicate beside a handler, because a
//! capability that answered yes and then could not cope, and a pair edited out
//! of step, are states that cannot be written this way.
//!
//! Tool calls and commands are offered around by [`Capabilities::offer`]; a child's outcome
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
/// session.
///
/// A struct rather than a tuple because both halves are lists and a tuple of
/// two `Vec`s reads the same in either order — the one shape where getting it
/// backwards compiles.
#[derive(Debug, Default)]
pub struct Decision {
    pub events: Vec<CapEvent>,
    pub actions: Vec<Action>,
}

impl Decision {
    /// Journal these, do nothing.
    #[must_use]
    pub fn record(events: Vec<CapEvent>) -> Self {
        Self {
            events,
            actions: Vec::new(),
        }
    }

    /// Answer the model, journal nothing. A refusal is not a fact about the
    /// session, so it must not reach the log.
    #[must_use]
    pub fn reply(text: impl Into<String>) -> Self {
        Self {
            events: Vec::new(),
            actions: vec![Action::Reply { text: text.into() }],
        }
    }

    #[must_use]
    pub fn then(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }
}

/// Why a capability could not equip the agent.
///
/// `fatal` is the capability's own call, and it is the whole answer to "does a
/// failed setup stop the turn?": the runtime says yes, because an agent with no
/// sandbox can do nothing; MCP says no, because a server that will not connect
/// costs the agent some tools and not its turn. Neither the session nor the
/// runner has to know which is which.
#[derive(Debug)]
pub struct SetupError {
    pub capability: &'static str,
    pub reason: String,
    pub fatal: bool,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} could not equip the agent: {}",
            self.capability, self.reason
        )
    }
}

/// One thing an agent can do.
///
/// `dyn` rather than an enum because a runner composes its list at runtime —
/// a workflow's step capabilities are built per step, from what that step
/// declared — so the set is not knowable at the point a match arm would have to
/// be written. [`CapSlice`] carries persistence instead, which keeps the
/// journal typed without putting the enum back in the dispatch path.
#[async_trait::async_trait]
pub trait Capability: std::fmt::Debug + Send + Sync {
    /// Stable, and the key its events are routed by. An associated const would
    /// be nicer to read but makes the trait not dyn-compatible.
    fn name(&self) -> &'static str;

    /// Equip the agent: acquire what this capability needs, then fill in the
    /// part of the spec it answers for.
    ///
    /// Async, and run on the agent's own task rather than the session mailbox —
    /// acquiring a sandbox, scanning a workspace and connecting an MCP server
    /// are all slow, and a session that cannot answer a `List` while one agent
    /// starts is the shape this design exists to avoid.
    ///
    /// Called per agent, so a capability may equip one of its runner's agents
    /// and not another — a workflow step that declares itself interactive gets
    /// `ask_user`, and the next step does not.
    ///
    /// Reads config only, never the folded slice: the list this runs against is
    /// built fresh for the agent being started, while the folded copy stays
    /// with the session.
    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError>;

    /// Release what `setup` acquired. Runs when the agent is unloaded.
    async fn teardown(&self) {}

    /// `None` means "not mine".
    ///
    /// `&Message` rather than by value because the same message is offered to
    /// each capability until one takes it; the taker clones what it keeps.
    fn handle(&self, caller: Caller, msg: &Message) -> Option<Decision>;

    /// Fold one of my own events. Pure: no clock, no randomness, no id
    /// generation — those belong in `handle`, which is a decision rather than
    /// a replay.
    ///
    /// Every capability is offered every event, so an arm that is not mine is
    /// a no-op rather than an error.
    fn apply(&mut self, event: &CapEvent) {
        let _ = event;
    }

    /// Me, in the form the journal stores.
    fn save(&self) -> CapSlice;
}

/// One capability as it is persisted.
///
/// The whole capability rather than a durable-state extract, so a reload does
/// not depend on [`super::assemble`] reproducing the same config it produced
/// when the runner was created. A capability's config is a fact about the
/// runner, and facts about the runner belong in its slice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapSlice {
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

impl From<CapSlice> for Box<dyn Capability> {
    fn from(slice: CapSlice) -> Self {
        match slice {
            CapSlice::Runtime(c) => Box::new(c),
            CapSlice::Mcp(c) => Box::new(c),
            CapSlice::Memory(c) => Box::new(c),
            CapSlice::ControlPlane(c) => Box::new(c),
            CapSlice::AskUser(c) => Box::new(c),
            CapSlice::Title(c) => Box::new(c),
            CapSlice::SubAgent(c) => Box::new(c),
            CapSlice::Workflow(c) => Box::new(c),
            CapSlice::Fork(c) => Box::new(c),
            CapSlice::StepResult(c) => Box::new(c),
        }
    }
}

/// What an agent is equipped with, in the order tool calls are offered around.
///
/// A newtype so the list round-trips through the journal as `Vec<CapSlice>`
/// with no hydration step: what comes back is what went in, including config.
#[derive(Debug, Default)]
pub struct Capabilities(Vec<Box<dyn Capability>>);

impl Capabilities {
    #[must_use]
    pub fn new(caps: Vec<Box<dyn Capability>>) -> Self {
        Self(caps)
    }

    /// Add a capability at the open-namespace end.
    ///
    /// Only [`super::assemble`] should reach the end: the last capability
    /// answers for a namespace nobody can enumerate, so anything pushed after
    /// it is shadowed. A capability with a fixed tool name wants
    /// [`Self::push_front`].
    pub fn push(&mut self, cap: impl Capability + 'static) {
        self.0.push(Box::new(cap));
    }

    /// Add a capability at the fixed-name end, ahead of everything already
    /// here.
    ///
    /// What a per-agent capability needs. A workflow step's `submit_result` is
    /// added to a copy of its runner's list when the step agent starts, and
    /// `push` would put it behind the runtime capability — which claims every
    /// tool call it is offered, and would swallow the step's own result.
    pub fn push_front(&mut self, cap: impl Capability + 'static) {
        self.0.insert(0, Box::new(cap));
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Box<dyn Capability>> {
        self.0.iter()
    }

    /// True only for a runner that owns no agents, so there is nothing to equip
    /// and nothing that could send it a tool call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn last(&self) -> Option<&dyn Capability> {
        self.0.last().map(AsRef::as_ref)
    }

    /// Whether a capability of this name is equipped.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|c| c.name() == name)
    }

    /// Offer a message to each capability until one takes it.
    ///
    /// `None` from all of them is an error at the one place this is called,
    /// never a silent drop.
    #[must_use]
    pub fn offer(&self, caller: Caller, msg: &Message) -> Option<Decision> {
        self.0.iter().find_map(|c| c.handle(caller, msg))
    }

    /// Fold a capability's event into the capability that owns it.
    pub fn apply(&mut self, event: &CapEvent) {
        for cap in &mut self.0 {
            cap.apply(event);
        }
    }

    /// Equip an agent by folding every capability over a fresh spec.
    ///
    /// One fold, one source, and no way to advertise a tool whose result
    /// nothing can process. Non-fatal failures are returned alongside the spec
    /// rather than swallowed: the agent starts, and the caller reports what it
    /// starts without.
    pub async fn equip(
        &self,
        settings: crate::sessions::spec::AgentSettings,
    ) -> Result<(AgentSpec, Vec<SetupError>), SetupError> {
        let mut spec = AgentSpec {
            settings: Some(settings),
            ..AgentSpec::default()
        };
        let mut degraded = Vec::new();
        for cap in &self.0 {
            if let Err(e) = cap.setup(&mut spec).await {
                if e.fatal {
                    return Err(e);
                }
                degraded.push(e);
            }
        }
        Ok((spec, degraded))
    }

    /// Release everything `equip` acquired.
    pub async fn teardown(&self) {
        for cap in &self.0 {
            cap.teardown().await;
        }
    }
}

impl Clone for Capabilities {
    /// Through the persisted form, so a clone cannot diverge from what a reload
    /// would produce.
    fn clone(&self) -> Self {
        Self(self.0.iter().map(|c| c.save().into()).collect())
    }
}

impl Serialize for Capabilities {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0
            .iter()
            .map(|c| c.save())
            .collect::<Vec<_>>()
            .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(
            Vec::<CapSlice>::deserialize(d)?
                .into_iter()
                .map(Into::into)
                .collect(),
        ))
    }
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

/// Parse a tool call's arguments, or answer the model with what was wrong.
///
/// A capability that owns a tool name owns every call to it, including the
/// malformed ones. Returning `None` on a parse failure would let the call fall
/// through to the next capability — and the last one is the open-namespace
/// runtime, which claims anything — so a mistyped `spawn_agent` would be
/// quietly absorbed by the sandbox layer instead of being corrected.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(
    tool: &str,
    input: &serde_json::Value,
) -> Result<T, Decision> {
    serde_json::from_value(input.clone()).map_err(|e| {
        Decision::reply(format!(
            "`{tool}` was called with arguments it cannot read: {e}"
        ))
    })
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

    fn caps(list: Vec<Box<dyn Capability>>) -> Capabilities {
        Capabilities::new(list)
    }

    /// A fixed-name capability wins over the open-namespace one behind it.
    /// Order is the conflict resolution for tool calls, so it is a property of
    /// assembly and gets a test rather than a comment.
    #[test]
    fn a_fixed_name_capability_beats_the_fallback_behind_it() {
        let caps = caps(vec![
            Box::new(title::TitleCapability::default()),
            Box::new(runtime::RuntimeCapability),
        ]);
        let d = caps
            .offer(
                caller(),
                &tool("set_session_title", serde_json::json!({"title": "x"})),
            )
            .expect("someone takes it");
        assert!(matches!(d.events.first(), Some(CapEvent::Title(_))));
    }

    /// And the same set in the wrong order routes it to the fallback, which is
    /// exactly the silent shadowing the written order exists to prevent.
    #[test]
    fn the_wrong_order_lets_the_fallback_shadow_a_named_tool() {
        let caps = caps(vec![
            Box::new(runtime::RuntimeCapability),
            Box::new(title::TitleCapability::default()),
        ]);
        let d = caps
            .offer(
                caller(),
                &tool("set_session_title", serde_json::json!({"title": "x"})),
            )
            .expect("the fallback takes it");
        assert!(
            d.events.is_empty(),
            "the runtime capability journals nothing, so the title was lost"
        );
    }

    /// A call nobody claims is `None` at the one place the scan lives.
    #[test]
    fn a_call_nobody_claims_is_none() {
        let caps = caps(vec![Box::new(title::TitleCapability::default())]);
        assert!(
            caps.offer(caller(), &tool("nope", serde_json::json!({})))
                .is_none()
        );
    }

    /// Equipping folds every capability over one spec, so what an agent can do
    /// is the sum of its runner's capabilities and nothing else.
    #[tokio::test]
    async fn equipping_folds_every_capability() {
        let caps = caps(vec![
            Box::new(title::TitleCapability::default()),
            Box::new(runtime::RuntimeCapability),
        ]);
        let (spec, degraded) = caps.equip(settings()).await.expect("nothing fatal");
        assert!(degraded.is_empty());
        assert!(spec.has(&crate::sessions::runners::action::ToolLayer::SessionTitle));
        assert!(spec.has(&crate::sessions::runners::action::ToolLayer::Runtime));
    }

    /// A capability's name is what its events are routed by and what a test
    /// asserts on, so the set is pinned here: renaming one is a deliberate act
    /// with a journal migration behind it, not a rename-symbol away.
    #[test]
    fn the_capability_names_are_pinned() {
        let all: Vec<Box<dyn Capability>> = vec![
            Box::new(runtime::RuntimeCapability),
            Box::new(mcp::McpCapability::new(vec!["s".into()])),
            Box::new(memory::MemoryCapability::new(vec!["m".into()])),
            Box::new(control_plane::ControlPlaneCapability),
            Box::new(ask_user::AskUserCapability::default()),
            Box::new(title::TitleCapability::default()),
            Box::new(sub_agent::SubAgentCapability::new(settings())),
            Box::new(workflow::WorkflowCapability::default()),
            Box::new(fork::ForkCapability::new(settings())),
            Box::new(step_result::StepResultCapability::default()),
        ];
        let names: Vec<&str> = all.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec![
                "runtime",
                "mcp",
                "memory",
                "control_plane",
                "ask_user",
                "title",
                "sub_agent",
                "workflow",
                "fork",
                "step_result",
            ]
        );
        // And every one of them round-trips through the persisted form, which
        // is the only thing keeping a reload from losing a capability.
        let round: Capabilities =
            serde_json::from_str(&serde_json::to_string(&Capabilities::new(all)).expect("write"))
                .expect("read");
        assert_eq!(
            round.iter().map(|c| c.name()).collect::<Vec<_>>(),
            names,
            "a capability was dropped or reordered by the journal"
        );
    }

    /// Cloning goes through `save()`, and `Action::StartAgent` clones the list
    /// every time an agent starts — so a `save()` that rebuilt itself from
    /// config instead of copying itself would silently drop what the runner had
    /// folded. Pinning the names cannot catch that; only folded state can.
    #[test]
    fn cloning_carries_the_folded_state_and_not_just_the_config() {
        let mut caps = Capabilities::new(vec![Box::new(title::TitleCapability::default())]);
        caps.apply(&CapEvent::Title(title::Event::Set {
            name: "the flake".into(),
        }));

        let copy = caps.clone();
        let CapSlice::Title(titled) = copy.iter().next().expect("one").save() else {
            panic!("the clone changed which capability this is");
        };
        assert_eq!(
            titled.title.as_deref(),
            Some("the flake"),
            "the clone was rebuilt from config and lost what the runner folded"
        );
    }

    /// A per-agent capability is added at the fixed-name end. Appended instead,
    /// it would sit behind the capability that claims every call it is offered
    /// — so the tool would be advertised and its calls answered by the sandbox.
    #[test]
    fn push_front_puts_a_fixed_name_ahead_of_the_open_namespace() {
        let mut caps = Capabilities::new(vec![Box::new(runtime::RuntimeCapability)]);
        caps.push_front(step_result::StepResultCapability::default());

        let taker = caps.iter().find_map(|c| {
            c.handle(caller(), &tool("submit_result", serde_json::json!({})))
                .map(|_| c.name())
        });
        assert_eq!(
            taker,
            Some("step_result"),
            "the sandbox layer swallowed the step's own result"
        );
    }
}
