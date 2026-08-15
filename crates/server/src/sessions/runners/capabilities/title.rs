//! `set_session_title`: the agent naming the conversation it is in.
//!
//! A conversation's own capability. A fork holds one too and names *itself*,
//! not the session it lives in — the model should not have to know which kind
//! of conversation it is in to name it.

use super::{CapEvent, CapSlice, Capability, Decision, SetupError};
use crate::sessions::runners::action::{AgentSpec, PromptSection, ToolLayer};
use crate::sessions::runners::ids::RunnerId;
use crate::sessions::runners::message::{Caller, Message};
use serde::{Deserialize, Serialize};

/// The tool this capability answers for.
pub const TOOL: &str = "set_session_title";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleCapability {
    /// The name recorded so far, if any.
    pub title: Option<String>,
    /// The fork this names, if it names one. `None` titles the session.
    pub fork: Option<RunnerId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Set { name: String },
}

/// The tool's arguments. Deserialised here so the schema and this type are one
/// declaration rather than two that can drift.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    pub title: String,
}

impl TitleCapability {
    #[must_use]
    pub fn for_fork(fork: RunnerId) -> Self {
        Self {
            title: None,
            fork: Some(fork),
        }
    }
}

#[async_trait::async_trait]
impl Capability for TitleCapability {
    fn name(&self) -> &'static str {
        "title"
    }

    async fn setup(&self, spec: &mut AgentSpec) -> Result<(), SetupError> {
        spec.layers.push(match self.fork {
            Some(fork) => ToolLayer::ForkTitle { fork },
            None => ToolLayer::SessionTitle,
        });
        spec.prompt.push(PromptSection {
            key: "title",
            body: "Name this conversation with `set_session_title` once you \
                   know what it is about."
                .to_string(),
        });
        Ok(())
    }

    fn handle(&self, _caller: Caller, msg: &Message) -> Option<Decision> {
        let Message::Tool(t) = msg else { return None };
        if t.name != TOOL {
            return None;
        }
        let req: Request = match super::parse(&t.name, &t.input) {
            Ok(req) => req,
            Err(refusal) => return Some(refusal),
        };
        Some(Decision {
            events: vec![CapEvent::Title(Event::Set {
                name: req.title.clone(),
            })],
            actions: vec![crate::sessions::runners::action::Action::Reply {
                text: format!("titled: {}", req.title),
            }],
        })
    }

    fn apply(&mut self, event: &CapEvent) {
        if let CapEvent::Title(Event::Set { name }) = event {
            self.title = Some(name.clone());
        }
    }

    fn save(&self) -> CapSlice {
        CapSlice::Title(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;

    #[test]
    fn setting_a_title_records_it() {
        let mut c = TitleCapability::default();
        let d = c
            .handle(
                caller(),
                &tool(TOOL, serde_json::json!({"title": "the flake"})),
            )
            .expect("mine");
        c.apply(&d.events[0]);
        assert_eq!(c.title.as_deref(), Some("the flake"));
    }

    /// A fork names itself. Same capability, different layer — which is what
    /// stops a fork renaming the session it branched from.
    #[tokio::test]
    async fn a_forks_capability_equips_the_fork_layer() {
        let fork = RunnerId::new_v4();
        let mut spec = AgentSpec::default();
        TitleCapability::for_fork(fork)
            .setup(&mut spec)
            .await
            .expect("nothing to acquire");
        assert!(spec.has(&ToolLayer::ForkTitle { fork }));
        assert!(!spec.has(&ToolLayer::SessionTitle));
    }

    #[test]
    fn another_tool_is_not_mine() {
        assert!(
            TitleCapability::default()
                .handle(caller(), &tool("bash", serde_json::json!({})))
                .is_none()
        );
    }
}
